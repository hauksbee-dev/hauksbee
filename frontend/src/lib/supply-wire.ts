// Mapping between the PowerPanel's UI-side supply config and the server's
// tagged wire enum (protocol.rs `PowerSupplyConfig`, `#[serde(tag = "kind",
// rename_all = "snake_case")]`). Extracted from PowerPanel so the wire shapes
// are unit-testable: sending the raw UI object used to fail serde
// deserialization on the server and the panel silently did nothing.

import type { PowerSupplyWire, UsbSpecWire } from '../types/protocol'

export type SupplyType = 'Ideal' | 'Bench' | 'Wall' | 'USB' | 'Battery'

export interface SupplyConfig {
  type: SupplyType
  volts: number
  currentLimit: number // A
  ripple: number       // Vpp
  capacity: number     // Ah (Battery only)
}

/** The USB spec closest to (but not under, when possible) the requested limit. */
export function usbSpecFor(currentLimitA: number): UsbSpecWire {
  if (currentLimitA <= 0.5) return 'v5_0_5a'
  if (currentLimitA <= 1.5) return 'v5_1_5a'
  return 'v5_3a'
}

/** Map a UI supply config onto the exact wire shape the server deserializes.
 *  Fields the UI has no knob for use the same defaults hauksbee-ci's spec
 *  layer applies (wall: 0.5 Ω / 100 Hz; battery: 1 cell li-ion, full SoC,
 *  0.1 Ω internal). */
export function toWireSupply(cfg: SupplyConfig): PowerSupplyWire {
  switch (cfg.type) {
    case 'Ideal':
      return { kind: 'ideal', volts: cfg.volts }
    case 'Bench':
      return { kind: 'bench', volts: cfg.volts, current_limit_a: cfg.currentLimit }
    case 'Wall':
      return {
        kind: 'wall',
        volts: cfg.volts,
        r_out_ohms: 0.5,
        ripple_vpp: cfg.ripple,
        ripple_hz: 100,
      }
    case 'USB':
      return { kind: 'usb', spec: usbSpecFor(cfg.currentLimit) }
    case 'Battery':
      return {
        kind: 'battery',
        chemistry: 'li_ion',
        cells: 1,
        capacity_mah: cfg.capacity * 1000,
        soc: 1.0,
        r_internal_ohms: 0.1,
      }
  }
}

/** A single li-ion cell's open-circuit voltage at full charge, mirroring the
 *  engine's chemistry curve (power_supply.rs, Chemistry::LiIon at SoC 1.0).
 *  Kept beside `toWireSupply`, which is what pins the battery to one li-ion
 *  cell in the first place. */
export const LI_ION_FULL_V = 4.2

/** USB VBUS, whichever current spec `usbSpecFor` picks. */
export const USB_VBUS_V = 5

/** The voltage this supply type will ACTUALLY apply, and, when the type sets
 *  its own voltage, the reason it ignores the box.
 *
 *  `toWireSupply` drops `volts` for USB and Battery (a USB spec is 5 V by
 *  definition; a battery sits at its chemistry's cell voltage), so typing 12
 *  into the voltage box used to leave the box reading 12 while the rail ran at
 *  5.000 V with nothing said. The panel asks here instead of assuming the box
 *  is the setpoint. */
export function appliedVolts(cfg: SupplyConfig): { volts: number; fixedBy?: string } {
  switch (cfg.type) {
    case 'USB':
      return { volts: USB_VBUS_V, fixedBy: 'USB VBUS is a fixed 5 V' }
    case 'Battery':
      return {
        volts: LI_ION_FULL_V,
        fixedBy: 'set by the cell: one li-ion cell, 4.2 V full, falling as it discharges',
      }
    default:
      return { volts: cfg.volts }
  }
}

/** Supply net names from BoardInfo.power_supplies, a net→config MAP on the
 *  wire, not an array. Iterating it with `for..of` was a TypeError that took
 *  the whole app down on any board with configurable supplies. */
export function supplyNetNames(
  powerSupplies: Record<string, unknown> | undefined | null,
): string[] | null {
  if (!powerSupplies || typeof powerSupplies !== 'object') return null
  const names = Object.keys(powerSupplies)
  return names.length > 0 ? names : null
}
