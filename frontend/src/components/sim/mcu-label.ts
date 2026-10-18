import { labelFor } from '../../lib/labels'

/** An MCU's emulation backend id ("simavr:atmega328p") in words, from the one
 *  shared label table (lib/labels.ts, generated from hauksbee-ir): "ATmega328P
 *  (simavr)". The raw id stays in the chip's tooltip, where it is the contract. */
export function mcuBackendText(backend: string): string {
  return labelFor(backend)
}
