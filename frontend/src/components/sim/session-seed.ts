import { parse as parseToml } from 'smol-toml'
import type { ChemistryWire, PowerSupplyWire, UsbSpecWire } from '../../types/protocol'
import type { WebReport } from '../../types/report'
import { machineFindings } from '../../lib/result-links'
import { labelFor } from '../../lib/labels'
import type { BenchReport } from '../../types/bench'

// What Live Sim takes over from the analysis on screen, so it continues that
// analysis instead of starting cold: the checks spec's supplies and duration,
// the nets the checks and findings name (pre-attached as probes), and what the
// static co-sim already saw (UART text, pin toggles).
//
// The spec's `[[supply]]` legs are mapped onto the live wire shape with the
// same defaults hauksbee-ci's runner applies (`build_supply` in
// crates/hauksbee-ci/src/runner.rs). No server route serves that mapping yet;
// this module is the one TS copy of it.

export interface SeedProbe {
  net: string
  /** Why it is attached, said on the probe row. */
  why: string
}

export interface SimSeed {
  /** The spec's supplies, as the live engine takes them. */
  supplies: { net: string; wire: PowerSupplyWire }[]
  /** The spec's `duration_ms`, in seconds; null without a spec. */
  durationS: number | null
  probes: SeedProbe[]
  /** What the analysis's static co-sim saw; null when it did not run. */
  cosim: { seconds: number; uart: string; toggles: { net: string; toggles: number }[] } | null
}

/** Probes attached up front; more is a cluttered scope nobody asked for. */
const MAX_SEED_PROBES = 6

const USB: Record<string, UsbSpecWire> = {
  '5v0.5a': 'v5_0_5a', '5v_0.5a': 'v5_0_5a',
  '5v1.5a': 'v5_1_5a', '5v_1.5a': 'v5_1_5a',
  '5v3a': 'v5_3a', '5v_3a': 'v5_3a',
}

const CHEMISTRY: Record<string, ChemistryWire> = {
  liion: 'li_ion', li_ion: 'li_ion', alkaline: 'alkaline', nimh: 'ni_mh', ni_mh: 'ni_mh',
  lifepo4: 'li_fe_po4', li_fe_po4: 'li_fe_po4',
}

const num = (v: unknown): number | undefined => (typeof v === 'number' && Number.isFinite(v) ? v : undefined)

/** One `[[supply]]` table as the live wire shape, or null when the spec leaves
 *  out what the CLI would refuse it for (no silent 5 V guess). */
export function supplyWire(s: Record<string, unknown>): PowerSupplyWire | null {
  const volts = num(s.volts)
  switch (String(s.kind ?? 'ideal')) {
    case 'ideal': return volts === undefined ? null : { kind: 'ideal', volts }
    case 'bench': return volts === undefined ? null : { kind: 'bench', volts, current_limit_a: num(s.current_limit_a) ?? 1.0 }
    case 'wall': return volts === undefined ? null : {
      kind: 'wall', volts, r_out_ohms: num(s.r_out_ohms) ?? 0.5,
      ripple_vpp: num(s.ripple_vpp) ?? 0.1, ripple_hz: num(s.ripple_hz) ?? 100,
    }
    case 'usb': {
      const spec = USB[String(s.usb ?? '')]
      return spec ? { kind: 'usb', spec } : null
    }
    case 'battery': {
      const chemistry = CHEMISTRY[String(s.chemistry ?? '').toLowerCase()]
      return chemistry ? {
        kind: 'battery', chemistry, cells: num(s.cells) ?? 1, capacity_mah: num(s.capacity_mah) ?? 1000,
        soc: num(s.soc) ?? 1, r_internal_ohms: num(s.r_internal_ohms) ?? 0.1,
      } : null
    }
    default: return null
  }
}

/** The spec's supplies, duration and asserted nets; empty for unreadable TOML. */
export function readSpec(toml: string | null | undefined): {
  supplies: SimSeed['supplies']; durationS: number | null; assertNets: string[]
} {
  if (!toml) return { supplies: [], durationS: null, assertNets: [] }
  let doc: Record<string, unknown>
  try {
    doc = parseToml(toml) as Record<string, unknown>
  } catch {
    return { supplies: [], durationS: null, assertNets: [] }
  }
  const supplies: SimSeed['supplies'] = []
  for (const s of (doc.supply as Record<string, unknown>[] | undefined) ?? []) {
    const wire = supplyWire(s)
    if (typeof s.net === 'string' && s.net && wire) supplies.push({ net: s.net, wire })
  }
  const ms = num(doc.duration_ms)
  const assertNets = ((doc.assert as Record<string, unknown>[] | undefined) ?? [])
    .map(a => a.net).filter((n): n is string => typeof n === 'string' && n.length > 0)
  // hauksbee-ci's own default when the spec names none (spec.rs).
  return { supplies, durationS: (ms ?? 100) / 1000, assertNets }
}

// ── The nets a person clips a probe to first ─────────────────────────────

const RAIL_NAME = /^[/~]?(?:[a-z]?gnd|vss|vdd|vcc|vbus|vbat|vin|[+-]\d)/i

/** Nets an indicator sits on: a part that is an LED (by library id, value or
 *  designator), the net a series resistor ties to it, and any net named for one. */
export function indicatorNets(report: WebReport): Set<string> {
  const leds = new Set<string>()
  const parts = report.model_coverage?.components ?? []
  // An LED's other leg sits on a rail, and a rail is not an indicator.
  const supplies = new Set((report.supplies ?? []).map(s => s.net))
  const rail = (n: string) => RAIL_NAME.test(n) || supplies.has(n)
  for (const c of parts) {
    const isLed = /\bLED\b/i.test(`${c.lib_id} ${c.value} ${c.footprint}`) || /^LED\d*$/i.test(c.reference)
    if (isLed) for (const p of c.pins) if (p.net && !rail(p.net)) leds.add(p.net)
  }
  // The MCU rarely drives the LED itself: a resistor sits between the two.
  for (const c of parts) {
    if (!/^R\d/i.test(c.reference) || c.pins.length !== 2) continue
    const [a, b] = [c.pins[0].net, c.pins[1].net]
    if (a && b && leds.has(a) !== leds.has(b)) {
      const other = leds.has(a) ? b : a
      if (!rail(other)) leds.add(other)
    }
  }
  for (const n of report.nets ?? []) if (/(^|[^a-z])led/i.test(n)) leds.add(n)
  return leds
}

const TOKEN_END = String.raw`(?:$|[^a-z0-9])`
const TOKEN_START = String.raw`(?:^|[^a-z0-9])`
const named = (words: string[]) => new RegExp(`${TOKEN_START}(?:${words.join('|')})\\d*${TOKEN_END}`, 'i')
const I2C_LINE = named(['scl', 'sda'])
const SPI_LINE = named(['sck', 'sclk', 'mosi', 'miso'])
const RESET_LINE = /^[/~]?(?:n?rst|nreset|reset|rst_?n|n_?rst)(?:$|[^a-z0-9])/i

/** The probes the bench report suggests, in the order a person reaches for
 *  them: pins that toggle (an indicator first), the serial transmit line, the
 *  bus lines of any bus that carried traffic, the reset line of a board that
 *  restarted. Each carries the reason, in the label table's words. */
export function benchSeeds(report: WebReport, bench: BenchReport | null | undefined): SeedProbe[] {
  if (!bench) return []
  const out: SeedProbe[] = []
  const add = (net: string | null | undefined, why: string) => {
    if (net && !out.some(p => p.net === net)) out.push({ net, why })
  }
  const indicators = indicatorNets(report)
  const pins = bench.mcus.flatMap(m => m.pins)
  const toggling = pins.filter(p => p.state === 'pin_toggling' && p.net)
  const sorted = [...toggling.filter(p => indicators.has(p.net!)), ...toggling.filter(p => !indicators.has(p.net!))]
  for (const p of sorted) {
    add(p.net, `${labelFor(indicators.has(p.net!) ? 'probe_seed_led' : 'probe_seed_pwm')}: ${p.pin}, ${p.summary}`)
  }
  for (const u of bench.mcus.flatMap(m => m.uarts ?? [])) add(u.net, `${labelFor('probe_seed_uart_tx')}: ${u.port}`)
  const nets = report.nets ?? []
  const kinds = new Set((bench.bus_log ?? []).map(t => t.bus))
  const busLines = [
    ...(kinds.has('bus_i2c') ? nets.filter(n => I2C_LINE.test(n)) : []),
    ...(kinds.has('bus_spi') ? nets.filter(n => SPI_LINE.test(n)) : []),
  ]
  for (const n of busLines) add(n, labelFor('probe_seed_bus'))
  const restarted = bench.mcus.some(m => m.resets > 0)
    || (bench.events ?? []).some(e => e.kind === 'event_reset' || e.kind === 'event_watchdog_reset')
  if (restarted) for (const n of nets.filter(n => RESET_LINE.test(n))) add(n, labelFor('probe_seed_reset'))
  return out
}

/** Bench seeds kept ahead of the findings' nets: the outputs, before the rest. */
const MAX_BENCH_SEEDS = 4

export function seedFrom(report: WebReport | null | undefined, specToml: string | null | undefined): SimSeed {
  const spec = readSpec(specToml)
  const probes: SeedProbe[] = []
  const add = (net: string, why: string) => {
    if (!probes.some(p => p.net === net)) probes.push({ net, why })
  }
  for (const net of spec.assertNets) add(net, 'a check names it')
  if (report) for (const s of benchSeeds(report, report.cosim?.ran ? report.cosim.bench : null).slice(0, MAX_BENCH_SEEDS)) add(s.net, s.why)
  if (report) for (const f of machineFindings(report)) for (const net of f.nets ?? []) add(net, 'a finding names it')
  const cosim = report?.cosim?.ran ? report.cosim : null
  const toggles = (cosim?.gpio_nets ?? [])
    .filter(g => (g.toggles ?? 0) > 0)
    .map(g => ({ net: g.name, toggles: g.toggles ?? 0 }))
  for (const t of toggles) add(t.net, `toggled ${t.toggles}x in the analysis`)
  return {
    supplies: specToml ? spec.supplies : [],
    durationS: specToml ? spec.durationS : null,
    probes: probes.slice(0, MAX_SEED_PROBES),
    cosim: cosim ? { seconds: Number(cosim.seconds_simulated.toPrecision(3)), uart: cosim.uart_output, toggles } : null,
  }
}

/** A supply in a few words: "Bench supply, 5 V, 2 A limit". The kind, USB
 *  profile and chemistry words come from the label table, so the spec's
 *  spellings (`bench`, `v5_1_5a`, `li_ion`) never reach a reader. */
export function supplyLabel(w: PowerSupplyWire | undefined): string {
  if (!w) return 'not set'
  switch (w.kind) {
    case 'ideal': return `${labelFor(w.kind)}, ${w.volts} V`
    case 'bench': return `${labelFor(w.kind)}, ${w.volts} V, ${w.current_limit_a} A limit`
    case 'wall': return `${labelFor(w.kind)}, ${w.volts} V, ${w.ripple_vpp} Vpp ripple`
    case 'usb': return `${labelFor(w.kind)}, ${labelFor(w.spec)}`
    case 'battery': return `${labelFor(w.kind)}, ${w.cells}x ${labelFor(w.chemistry)}, ${w.capacity_mah} mAh`
  }
}

/** Two supplies that would run the same rail. */
export function sameSupply(a: PowerSupplyWire | undefined, b: PowerSupplyWire | undefined): boolean {
  if (!a || !b) return a === b
  const keys = new Set([...Object.keys(a), ...Object.keys(b)])
  return [...keys].every(k => (a as unknown as Record<string, unknown>)[k] === (b as unknown as Record<string, unknown>)[k])
}
