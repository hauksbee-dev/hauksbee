import { expect, test } from 'bun:test'
import { supplyLabel } from '../src/components/sim/session-seed'
import { SUPPLY_KINDS, USB_PROFILES } from '../src/lib/check-spec'
import { labelFor } from '../src/lib/labels'

// A supply is named in the reader's words: the kind, the USB profile and the
// battery chemistry come from the label table, never as the spec's spellings.

test('a supply reads as words, not as the kind id', () => {
  expect(supplyLabel(undefined)).toBe('not set')
  expect(supplyLabel({ kind: 'ideal', volts: 5 })).toBe('Ideal source, 5 V')
  expect(supplyLabel({ kind: 'bench', volts: 5, current_limit_a: 2 })).toBe('Bench supply, 5 V, 2 A limit')
  expect(supplyLabel({ kind: 'usb', spec: 'v5_1_5a' })).toBe('USB port, 5 V, 1.5 A')
  expect(supplyLabel({ kind: 'usb', spec: 'v5_0_5a' })).toBe('USB port, 5 V, 0.5 A')
  expect(supplyLabel({
    kind: 'battery', chemistry: 'li_ion', cells: 2, capacity_mah: 2000, soc: 1, r_internal_ohms: 0.1,
  })).toBe('Battery, 2x Li-ion, 2000 mAh')
})

test('every supply kind and USB profile the spec builder offers has words of its own', () => {
  for (const id of [...SUPPLY_KINDS, ...USB_PROFILES]) {
    expect(labelFor(id)).not.toBe(id)
    expect(labelFor(id)).not.toMatch(/^[a-z0-9._-]+$/)
  }
})
