// Plain names for an MCU's emulation backend. The wire carries
// "<emulator>:<chip>" (hauksbee-bind's backend string, e.g.
// "simavr:atmega328p"); that is an internal id, not something to put in front of
// a user. Until the shared label table (frontend/src/lib/labels.ts) lands, this
// is the one place the live view turns it into words.

const EMULATORS: Record<string, string> = {
  simavr: 'AVR emulator',
  renode: 'Renode emulator',
  qemu: 'QEMU emulator',
}

/** "atmega328p" -> "ATmega328P", "stm32f072" -> "STM32F072", "esp32c3" -> "ESP32-C3". */
export function chipName(chip: string): string {
  const c = chip.toLowerCase()
  const avr = /^(atmega|attiny)(\w+)$/.exec(c)
  if (avr) return `${avr[1] === 'atmega' ? 'ATmega' : 'ATtiny'}${avr[2].toUpperCase()}`
  const esp = /^esp32([a-z]\d)$/.exec(c)
  if (esp) return `ESP32-${esp[1].toUpperCase()}`
  return c.toUpperCase()
}

export interface McuBackendLabel {
  /** The chip: "ATmega328P". */
  chip: string
  /** How it is emulated: "AVR emulator". Empty for an id this table does not know. */
  how: string
}

export function mcuBackendLabel(backend: string): McuBackendLabel {
  const [emu, chip] = backend.includes(':') ? (backend.split(':', 2) as [string, string]) : ['', backend]
  return { chip: chipName(chip), how: EMULATORS[emu.toLowerCase()] ?? '' }
}

/** One line: "ATmega328P (AVR emulator)". */
export function mcuBackendText(backend: string): string {
  const { chip, how } = mcuBackendLabel(backend)
  return how ? `${chip} (${how})` : chip
}
