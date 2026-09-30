// A net powered by a regulator with no proven input headroom carries an ideal
// source's SETPOINT. Printing it as "3.300 V" is the exact false comfort this
// module exists to prevent, so the unsupported reading must outrank both the
// unobserved case and a perfectly ordinary measured number.
// Run with: bun test  (from frontend/)

import { test, expect } from 'bun:test'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { readNet, netReadoutText } from '../src/lib/net-state'
import { NetPanel } from '../src/components/NetPanel'
import { FaultPanel } from '../src/components/FaultPanel'
import { ProbeScopePanel } from '../src/components/ProbeScopePanel'
import type { SimFrame } from '../src/types/protocol'

const frame = {
  unsupported_nets: ['VOUT'],
  unsupported_reasons: ['U1 regulation of VOUT is unsupported: ...'],
  unobserved_drive_nets: ['VOUT', 'PB0'],
  net_voltages: { VOUT: 3.3, PB0: 0, VIN: 2.5 },
} as unknown as SimFrame

test('an unsupported net reads as unsupported, ahead of unobserved and measured', () => {
  expect(readNet(frame, 'VOUT')).toEqual({ kind: 'unsupported' })
  expect(readNet(frame, 'PB0')).toEqual({ kind: 'unobserved' })
  expect(readNet(frame, 'VIN').kind).toBe('measured')
})

test('the unsupported readout never prints a number', () => {
  const text = netReadoutText(readNet(frame, 'VOUT'))
  expect(text).toBe('unsupported: not a simulated value')
  expect(text).not.toContain('3.3')
})

// Every surface that prints a net, held to the same line: the number belongs to
// a solve that happened, or it is not printed at all.
test('no surface prints a number for a net nobody simulated', () => {
  const nets = renderToStaticMarkup(createElement(NetPanel, { frame, selectedNet: null, onSelectNet: () => undefined }))
  expect(nets).toContain('unsupported: not a simulated value')
  expect(nets).toContain('net-row-unmeasured')
  // VIN is measured and still reads as a number; only VOUT lost its bar.
  expect(nets).toContain('+2.500V')
  expect(nets).not.toContain('+0.000V')
  expect(nets).not.toContain('3.300V')

  const scope = renderToStaticMarkup(createElement(ProbeScopePanel, {
    boardInfo: null, frame, probes: ['VOUT', 'VIN'],
    onAddProbe: () => undefined, onRemoveProbe: () => undefined,
  }))
  expect(scope).toContain('unsupported: not a simulated value')
  expect(scope).not.toContain('3.300')
  // The long status truncates in the probe row; the name and the detach button keep their width.
  expect(scope).toContain('title="unsupported: not a simulated value"')

  // And a clean fault list says WHY part of the circuit was never watched.
  const faults = renderToStaticMarkup(createElement(FaultPanel, { faults: [], unsupportedReasons: frame.unsupported_reasons }))
  expect(faults).toContain('No faults detected in what is monitored')
  expect(faults).toContain('U1 regulation of VOUT is unsupported')
})

test('a frame from an older server without the field still reads as measured', () => {
  const old = { net_voltages: { VOUT: 3.3 } } as unknown as SimFrame
  expect(readNet(old, 'VOUT').kind).toBe('measured')
})
