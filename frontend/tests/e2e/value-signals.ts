/** Pure signal helpers for the release board journey.
 *
 * The journey itself needs a live browser, so nothing in it can be unit
 * tested. Everything here is the part that CAN be: parsing the transport
 * clock, parsing the firmware plan the gate hands over, and deciding whether a
 * finished report actually co-simulated the firmware it was given. The journey
 * imports these rather than carrying second copies.
 */

import { isAbsolute } from 'node:path'

/** What the gate pairs with one board, or null where it paired nothing. */
export interface FirmwarePlan {
  path: string
  /** `cosim`: the report must co-simulate it. `load-only`: the build has no
   *  target for this MCU, so the image must be identified and the reason
   *  named, but no co-sim is demanded. */
  expect: 'cosim' | 'load-only'
}

export interface CosimSectionLike {
  ran?: boolean
  seconds_simulated?: number
  uart_output?: string
  gpio_nets?: { name?: string; volts?: number; driven?: boolean }[]
  /** Not optional in the engine's co-sim section. A run that omits it must not
   *  grade better than one that declares the solve invalid, so the value
   *  contract requires it; see qc/value_grading.py. */
  analog_valid?: boolean
}

/** Read the rendered transport clock. Throws on anything it cannot read
 *  rather than returning a zero that would read as "no progress". */
export function parseSimTime(text: string): number {
  const match = text.trim().match(/^([0-9]+(?:\.[0-9]+)?)\s*(µs|ms|s)$/)
  if (!match) throw new Error(`unrecognised simulation time: ${JSON.stringify(text)}`)
  const value = Number(match[1])
  if (match[2] === 'µs') return value / 1_000_000
  if (match[2] === 'ms') return value / 1_000
  return value
}

/** Key-sorted JSON, so two reports compare on content and not on key order. */
export function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const object = value as Record<string, unknown>
    return `{${Object.keys(object)
      .sort()
      .map(key => `${JSON.stringify(key)}:${canonicalJson(object[key])}`)
      .join(',')}}`
  }
  return JSON.stringify(value)
}

/** Validate `HB_FIRMWARE_FILES` against the boards this run drops.
 *
 * The array runs parallel to the board list, so a plan of the wrong length is
 * a mis-wired gate rather than a board that happens to carry no firmware, and
 * it is refused instead of silently dropping a firmware expectation on the
 * floor.
 */
export function parseFirmwarePlan(raw: string, boardCount: number): (FirmwarePlan | null)[] {
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    throw new Error('HB_FIRMWARE_FILES must be valid JSON')
  }
  if (!Array.isArray(parsed)) throw new Error('HB_FIRMWARE_FILES must be a JSON array')
  if (parsed.length === 0) return new Array(boardCount).fill(null)
  if (parsed.length !== boardCount) {
    throw new Error(
      `HB_FIRMWARE_FILES has ${parsed.length} entries for ${boardCount} boards`,
    )
  }
  return parsed.map((entry, index) => {
    if (entry === null) return null
    if (typeof entry !== 'object') {
      throw new Error(`HB_FIRMWARE_FILES[${index}] must be null or an object`)
    }
    const item = entry as Record<string, unknown>
    const path = item.path
    const expect = item.expect ?? 'cosim'
    if (typeof path !== 'string' || !isAbsolute(path)) {
      throw new Error(`HB_FIRMWARE_FILES[${index}] needs an absolute path`)
    }
    if (expect !== 'cosim' && expect !== 'load-only') {
      throw new Error(`HB_FIRMWARE_FILES[${index}] has an unknown expect ${String(expect)}`)
    }
    return { path, expect }
  })
}

/** True when the co-sim drove at least one GPIO net. Kept separate from serial
 *  output on purpose: "a pin moved" and "the firmware printed something" are
 *  different observations, and a single boolean covering both would record pin
 *  activity for a run that only wrote to a UART. */
export function cosimDrovePin(cosim: CosimSectionLike): boolean {
  return (cosim.gpio_nets ?? []).some(net => net.driven === true)
}

/** True when the co-sim emitted serial output. */
export function cosimPrinted(cosim: CosimSectionLike): boolean {
  return typeof cosim.uart_output === 'string' && cosim.uart_output.trim() !== ''
}

/** Everything wrong with what a firmware-carrying report came back with.
 *  Empty means the report honoured the expectation it was given. */
export function cosimFailures(
  cosim: CosimSectionLike | null | undefined,
  expect: FirmwarePlan['expect'],
): string[] {
  const failures: string[] = []
  if (expect === 'load-only') {
    // Nothing is demanded of the run except that it did not silently pretend.
    // A build that CAN co-simulate this image is a pleasant surprise, not a
    // failure, so only an outright missing section is wrong here.
    if (cosim === null || cosim === undefined) {
      failures.push('firmware was staged but the report carried no co-sim section')
    }
    return failures
  }
  if (cosim === null || cosim === undefined) {
    failures.push('firmware was staged but the report carried no co-sim section')
    return failures
  }
  if (cosim.ran !== true) {
    failures.push('firmware was staged but the report co-simulated nothing')
    return failures
  }
  if (!(typeof cosim.seconds_simulated === 'number' && cosim.seconds_simulated > 0)) {
    failures.push('firmware co-sim reported zero simulated seconds')
  }
  // Pin activity and the analog verdict are recorded as signals and judged by
  // the value contract in qc/value_grading.py, which has the report's own
  // per-part unlocks to hand and can tell an inert co-sim (a failure: no upload
  // fixes it) from an invalidated analog solve (degraded: binding the open parts
  // does). Deciding either here would duplicate that judgement with less
  // information.
  return failures
}
