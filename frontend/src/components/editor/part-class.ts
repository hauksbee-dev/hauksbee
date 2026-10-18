// What kind of part is this, from what the board says about it?
//
// The model authoring form opens with a kind already chosen when the board's
// own evidence settles it, and says why in one sentence, so a BS250 the board
// names as a MOSFET is never offered as "a digital part". When the evidence does
// not settle it (a bare transistor symbol, an unknown IC) the answer is `null`
// and the author chooses: a wrong default is worse than a question.
//
// Evidence is read in a fixed order and the first rule that fires wins:
//   1. a known part number (the table below, a prefix match on the value or the
//      symbol name),
//   2. the symbol's library (`Transistor_FET:...`, `LED:...`, `Regulator_Linear:...`),
//   3. the reference designator's letters (R, C, L, D, J ...),
//   4. the footprint, only to say what family an undecided transistor is in.

/** The kinds a hand-written model can be, in the words someone with a datasheet
 *  in hand would use. A reference letter does not say (`Q` is a BJT or a MOSFET),
 *  so the author can always choose. */
export const KIND_CHOICES: [id: string, what: string][] = [
  ['passive', 'resistor, capacitor, inductor'], ['diode', 'diode, Schottky, Zener, LED'],
  ['bjt_npn', 'NPN transistor'], ['bjt_pnp', 'PNP transistor'],
  ['nmos', 'N-channel MOSFET'], ['pmos', 'P-channel MOSFET'],
  ['vreg', 'regulator, LDO, charger'], ['opamp', 'op-amp'], ['comparator', 'comparator'],
  ['timer', '555-class timer'], ['analog_switch', 'analogue or load switch'],
  ['digital', 'logic or digital part'], ['dac', 'DAC'], ['adc', 'ADC'],
  ['shift_register', 'shift register'], ['mcu', 'microcontroller'], ['connector', 'connector'],
]

const KIND_LABELS = new Map<string, string>([
  ...KIND_CHOICES,
  ['charger', 'battery charger'], ['pmic', 'power-management IC'], ['balancer', 'cell balancer or monitor'],
  ['i2c_sensor', 'I2C sensor'], ['spi_sensor', 'SPI sensor'], ['register_map', 'register-map device'],
  ['ignore', 'part left out on purpose'],
])

/** A kind id in plain words, for any surface outside the TOML view. An id the
 *  table does not know is shown with its underscores opened up, not raw. */
export function kindLabel(id: string): string {
  return KIND_LABELS.get(id) ?? id.replace(/_/g, ' ')
}

/** The one-word class of a part for sentences ("a MOSFET", "a regulator"). */
const FAMILY: Record<string, string> = {
  passive: 'passive part', diode: 'diode', bjt_npn: 'NPN transistor', bjt_pnp: 'PNP transistor',
  nmos: 'N-channel MOSFET', pmos: 'P-channel MOSFET', vreg: 'regulator', opamp: 'op-amp',
  comparator: 'comparator', timer: 'timer', analog_switch: 'switch', digital: 'logic part',
  dac: 'DAC', adc: 'ADC', shift_register: 'shift register', mcu: 'microcontroller', connector: 'connector',
}

export interface PartFacts {
  reference: string
  value: string
  lib_id?: string
  footprint?: string
  /** What the engine already calls it, when it has said. */
  model_kind?: string | null
  properties?: Array<{ name: string; value: string }>
}

export interface PartClass {
  /** The kind to start on, or null when the evidence does not settle it. */
  kind: string | null
  /** One sentence for the author: what was decided and from which evidence, or
   *  what is undecided and what to choose between. */
  why: string
}

// Part-number prefixes, lower case. Prefix match: "bs250" also claims "bs250p".
const PMOS = [
  'bs250', 'bss84', 'bss83', 'si2301', 'si2305', 'si2307', 'si2309', 'si2343', 'si2347', 'ao3401', 'ao3407', 'ao3409',
  'ao3413', 'ao3415', 'irf9', 'irfr9', 'irlml64', 'dmp', 'fdn30', 'ndp6', 'zvp', 'pmos', 'pfet', 'p-channel', 'p-ch',
]
const NMOS = [
  '2n7000', '2n7002', 'bss138', 'bss123', 'bs170', 'bs107', 'bsn20', 'irf5', 'irf6', 'irf7', 'irf8', 'irfz', 'irlz', 'irlb',
  'irlml2', 'irlml0', 'irlml6344', 'ao3400', 'ao3402', 'ao3404', 'ao3406', 'si2300', 'si2302', 'si2304', 'si2306', 'si2308',
  'dmn', 'fdn337', 'fdn339', 'fdn359', 'nmos', 'nfet', 'n-channel', 'n-ch', 'cj2302',
]
const NPN = ['bc847', 'bc817', 'bc337', 'bc546', 'bc547', 'bc548', 'bc549', 'bc550', '2n3904', '2n2222', '2n5551', 'mmbt3904', 'mmbt2222', 'pn2222', 's8050', 'mmbt5551', 'ss8050']
const PNP = ['bc857', 'bc807', 'bc327', 'bc556', 'bc557', 'bc558', 'bc559', '2n3906', '2n2907', '2n5401', 'mmbt3906', 'mmbt2907', 'pn2907', 's8550', 'mmbt5401', 'ss8550']

const startsWithAny = (haystacks: string[], prefixes: string[]) =>
  prefixes.find(p => haystacks.some(h => h === p || new RegExp(`(?:^|[^a-z0-9])${p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`).test(h)))

// IC families by part-number pattern.
const IC_RULES: [RegExp, string, string][] = [
  [/(ams1117|lm1117|ld1117|lm317|lm78\d\d|lm79\d\d|l78\d\d|ap2112|ap7\d{3}|xc6206|mcp1700|mic29\d|mic5\d{3}|tps7\d|lp29\d\d|ncp11\d\d|ncp7\d|mcp1[67]\d\d|ld39\d+|lm2940|ldo|regulator)/i, 'vreg', 'a voltage regulator'],
  [/(lm358|lm324|lm2904|tl07\d|tl08\d|opa\d|mcp60\d\d|mcp62\d\d|ne5532|lmv\d|ad8\d{3}|op\d{3}|ina\d{3})/i, 'opamp', 'an op-amp'],
  [/(lm393|lm339|lm311|tlv3\d{3}|lmv3\d\d|comparator)/i, 'comparator', 'a comparator'],
  [/(ne555|lm555|tlc555|ca555|\b555\b)/i, 'timer', 'a 555-class timer'],
  [/(74(hc|hct|lv|lvc|ahc|ac|act|ls|f|alvc|avc)?595|74hc164|74hc165|sn74.*595)/i, 'shift_register', 'a shift register'],
  [/(^|[^a-z0-9])(74(hc|hct|lv|lvc|ahc|ac|act|ls|f|alvc|avc)?\d{2,4}|sn74|cd40\d\d|cd45\d\d)/i, 'digital', 'a logic part'],
  [/(atmega|attiny|atxmega|stm32|stm8|esp32|esp8266|rp2040|rp2350|pic1\d|pic3\d|nrf5\d|samd\d|msp430|ch32|efm32|lpc\d|at90)/i, 'mcu', 'a microcontroller'],
  [/(ads1\d{3}|mcp3\d{3}|mcp33\d\d|ltc24\d\d|\badc)/i, 'adc', 'an ADC'],
  [/(mcp47\d\d|dac\d{4}|mcp48\d\d|\bdac)/i, 'dac', 'a DAC'],
  [/(tps229\d\d|ts5a\d+|cd4066|74hc4066|74hc4051|load.?switch|analog.?switch)/i, 'analog_switch', 'an analogue or load switch'],
]

/** The class of a part, from the board's own evidence. Never throws. */
export function inferPartClass(part: PartFacts): PartClass {
  const ref = part.reference.trim()
  const letters = (/^[A-Za-z]+/.exec(ref)?.[0] ?? '').toUpperCase()
  const value = part.value.trim().toLowerCase()
  const lib = (part.lib_id ?? '').trim()
  const libName = (lib.split(':').pop() ?? '').toLowerCase()
  const libTable = (lib.includes(':') ? lib.split(':')[0] : '').toLowerCase()
  const fp = (part.footprint ?? '').trim()
  const fpName = (fp.split(':').pop() ?? '').toLowerCase()
  const desc = (part.properties ?? []).map(p => p.value).join(' ').toLowerCase()
  const names = [value, libName].filter(Boolean)
  const seen = (...bits: [string, string | undefined][]) =>
    bits.filter(([, v]) => v && v.trim()).map(([label, v]) => `${label} ${v}`).join(', ')
  const evidence = seen(['reference', ref], ['symbol', lib], ['footprint', fp], ['part number', part.value.trim()])
  const decided = (kind: string, statement: string): PartClass => ({
    kind, why: `${statement} (from ${evidence || 'the board'}). Change it if that is wrong.`,
  })

  // 0. The engine already knows.
  if (part.model_kind && FAMILY[part.model_kind]) {
    return decided(part.model_kind, `The engine already treats ${ref || 'this part'} as a ${FAMILY[part.model_kind]}`)
  }

  // 1. Part numbers.
  const isFetLib = /fet|mosfet/.test(libTable) || /mosfet|fet/.test(libName)
  const isBjtLib = /bjt|transistor_bjt/.test(libTable)
  const p = startsWithAny(names, PMOS)
  if (p && (letters === 'Q' || isFetLib || p.includes('-') || p === 'pmos' || p === 'pfet')) {
    return decided('pmos', `${part.value.trim() || ref} is a P-channel MOSFET part`)
  }
  const n = startsWithAny(names, NMOS)
  if (n && (letters === 'Q' || isFetLib || n.includes('-') || n === 'nmos' || n === 'nfet')) {
    return decided('nmos', `${part.value.trim() || ref} is an N-channel MOSFET part`)
  }
  if (letters === 'Q' || isBjtLib) {
    if (startsWithAny(names, NPN)) return decided('bjt_npn', `${part.value.trim()} is an NPN transistor part`)
    if (startsWithAny(names, PNP)) return decided('bjt_pnp', `${part.value.trim()} is a PNP transistor part`)
  }
  if (/p-?channel|pmos/.test(desc)) return decided('pmos', 'The part description says P-channel MOSFET')
  if (/n-?channel|nmos/.test(desc)) return decided('nmos', 'The part description says N-channel MOSFET')

  // 2./3. Undecided transistors.
  if (isFetLib) {
    return { kind: null, why: `The symbol says MOSFET but not which polarity (from ${evidence}). Choose N-channel or P-channel from the datasheet.` }
  }
  if (isBjtLib) {
    return { kind: null, why: `The symbol says bipolar transistor but not NPN or PNP (from ${evidence}). Choose from the datasheet.` }
  }
  if (letters === 'Q') {
    const small = /sot-?23|to-?92|sot-?323|sc-?70|sot-?223|to-?220|dpak/.test(fpName)
    return {
      kind: null,
      why: `${ref} is a transistor${small ? ' in a transistor package' : ''} but nothing on the board says whether it is a MOSFET or a bipolar, or which polarity (from ${evidence}). Choose from the datasheet.`,
    }
  }

  // 4. Reference letters.
  if (letters === 'R' || letters === 'RN' || letters === 'RV' || letters === 'RT' || letters === 'VR') return decided('passive', `Reference ${ref} is a resistor`)
  if (letters === 'C' || letters === 'CP') return decided('passive', `Reference ${ref} is a capacitor`)
  if (letters === 'L') return decided('passive', `Reference ${ref} is an inductor`)
  if (letters === 'F' || letters === 'FB') return decided('passive', `Reference ${ref} is a fuse or ferrite bead`)
  if (letters === 'D' || letters === 'LED' || letters === 'ZD' || letters === 'DZ' || /^led/.test(libTable) || libTable === 'diode') {
    return decided('diode', `Reference ${ref} is a diode or LED`)
  }
  if (letters === 'J' || letters === 'P' || letters === 'CN' || letters === 'H' || /connector/.test(libTable)) {
    return decided('connector', `Reference ${ref} is a connector`)
  }

  // ICs by part number.
  const haystack = `${value} ${libName} ${desc}`
  for (const [re, kind, what] of IC_RULES) {
    if (re.test(haystack)) return decided(kind, `${part.value.trim() || ref} looks like ${what}`)
  }
  if (/regulator|power/.test(libTable)) return decided('vreg', `The symbol library (${lib}) is for regulators`)
  if (/amplifier_operational|op.?amp/.test(libTable)) return decided('opamp', `The symbol library (${lib}) is for op-amps`)
  if (/^74xx|logic/.test(libTable)) return decided('digital', `The symbol library (${lib}) is for logic`)
  if (/mcu|microcontroller/.test(libTable)) return decided('mcu', `The symbol library (${lib}) is for microcontrollers`)

  return {
    kind: null,
    why: `Nothing on the board says what ${ref || 'this part'} does (${evidence || 'no symbol or footprint was read'}). Choose what it is, or keep it open for now.`,
  }
}
