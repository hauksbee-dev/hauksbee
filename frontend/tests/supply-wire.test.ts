// Regression tests for the PowerPanel <-> server power-supply wire contract.
// The panel used to (a) iterate BoardInfo.power_supplies (a net->config MAP)
// as an array, throwing a TypeError that unmounted the whole app, and
// (b) send its raw UI config object as `supply`, which the server's tagged
// serde enum rejected — the panel silently did nothing.
// Run with: bun test  (from frontend/)

import { test, expect } from 'bun:test'
import { toWireSupply, supplyNetNames, usbSpecFor, appliedVolts } from '../src/lib/supply-wire'
import type { SupplyConfig } from '../src/lib/supply-wire'

const base: Omit<SupplyConfig, 'type'> = {
  volts: 5,
  currentLimit: 1.5,
  ripple: 0.1,
  capacity: 2,
}

test('supplyNetNames handles the wire map shape (not an array) without throwing', () => {
  // Exactly what protocol.rs BoardInfo serializes: a net -> tagged-config map.
  const wire = {
    '5V': { kind: 'ideal', volts: 5 },
    VBAT: { kind: 'battery', chemistry: 'li_ion', cells: 1, capacity_mah: 2000, soc: 1, r_internal_ohms: 0.1 },
  }
  expect(supplyNetNames(wire)).toEqual(['5V', 'VBAT'])
  expect(supplyNetNames(undefined)).toBeNull()
  expect(supplyNetNames(null)).toBeNull()
  expect(supplyNetNames({})).toBeNull() // server omits when empty; treat empty as absent
})

test('ideal/bench map to the tagged snake_case shapes serde expects', () => {
  expect(toWireSupply({ ...base, type: 'Ideal' })).toEqual({ kind: 'ideal', volts: 5 })
  expect(toWireSupply({ ...base, type: 'Bench' })).toEqual({
    kind: 'bench',
    volts: 5,
    current_limit_a: 1.5,
  })
})

test('wall carries the UI ripple and the CI-spec defaults for the rest', () => {
  expect(toWireSupply({ ...base, type: 'Wall' })).toEqual({
    kind: 'wall',
    volts: 5,
    r_out_ohms: 0.5,
    ripple_vpp: 0.1,
    ripple_hz: 100,
  })
})

test('usb picks the spec enum value from the current limit', () => {
  expect(usbSpecFor(0.3)).toBe('v5_0_5a')
  expect(usbSpecFor(0.5)).toBe('v5_0_5a')
  expect(usbSpecFor(1.5)).toBe('v5_1_5a')
  expect(usbSpecFor(3)).toBe('v5_3a')
  expect(toWireSupply({ ...base, type: 'USB' })).toEqual({ kind: 'usb', spec: 'v5_1_5a' })
})

test('battery converts Ah to mAh and fills the wire-required fields', () => {
  expect(toWireSupply({ ...base, type: 'Battery' })).toEqual({
    kind: 'battery',
    chemistry: 'li_ion',
    cells: 1,
    capacity_mah: 2000,
    soc: 1.0,
    r_internal_ohms: 0.1,
  })
})

test('appliedVolts reports what the rail will really run at, per supply type', () => {
  // Ideal/bench/wall pass the box through; USB and battery set their own
  // voltage, and `toWireSupply` drops `volts` for them entirely. The panel
  // showed 12 while the rail ran at 5.000 V with nothing said.
  expect(appliedVolts({ ...base, type: 'Ideal', volts: 12 })).toEqual({ volts: 12 })
  expect(appliedVolts({ ...base, type: 'Bench', volts: 12 })).toEqual({ volts: 12 })
  expect(appliedVolts({ ...base, type: 'Wall', volts: 12 })).toEqual({ volts: 12 })

  const usb = appliedVolts({ ...base, type: 'USB', volts: 12 })
  expect(usb.volts).toBe(5)
  expect(usb.fixedBy).toBeTruthy()

  // Matches the engine's li-ion curve at full charge (power_supply.rs).
  const batt = appliedVolts({ ...base, type: 'Battery', volts: 12 })
  expect(batt.volts).toBe(4.2)
  expect(batt.fixedBy).toBeTruthy()
})
