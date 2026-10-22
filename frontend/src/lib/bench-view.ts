// The bench strip's view-model: a co-sim run's observations (BenchReport) as the
// chips and headline the report shows, and as the lanes, decoded boxes and event
// rules the run view draws. Pure: no DOM, no canvas. The words for an id (a pin
// state, a core state, an event, a bus) come from the shared label table; the
// engine's own sentences (`headline`, `summary`) are shown as it wrote them.

import type { BenchEvent, BenchReport, BusTransaction, McuBench, PinRow, UartRow } from '../types/bench'
import { formatSimTime } from '../components/scope-draw'
import type { DecodedItem, ScopeLane } from '../components/bench-draw'
import { traceToken } from '../components/scope-draw'
import { initialLevel } from '../components/bench-draw'
import { labelFor } from './labels'

export type BenchTone = 'ok' | 'warn' | 'err'

/** One pin as a chip. */
export interface PinChipView {
  key: string
  pin: string
  /** The net it drives; a chip without one cannot be shown on the board. */
  net: string | null
  /** The engine's row summary: `toggling 5.0 Hz, 50% duty`. */
  summary: string
  /** A pin the firmware never drove: drawn muted. */
  muted: boolean
  /** True when the lane holds fewer edges than the pin had. */
  truncated: boolean
  /** The tooltip: what a click does, or why it does nothing. */
  title: string
}

/** One serial port as a row. */
export interface UartChipView {
  key: string
  port: string
  net: string | null
  /** `115200 baud`, or null when the backend cannot read the rate. */
  baud: string | null
  /** Why there is no rate, for the tooltip (never the chip: it is not news). */
  baudNote: string | null
  bytes: string
  firstLine: string | null
  /** Said when most bytes are not text, which is what a wrong baud looks like. */
  garbled: string | null
  title: string
}

export interface McuStripView {
  ref: string
  backend: string
  /** The core state in words, from the label table. */
  coreLabel: string
  /** The engine's one-line headline: `parked at Error_Handler`. */
  headline: string
  tone: BenchTone
  /** True when the headline is trouble and is drawn as an inline issue. */
  issue: boolean
  /** The details under an issue: where the core sits, since when. */
  detail: string[]
  /** Reset count and time to first activity. */
  facts: string[]
  pins: PinChipView[]
  uarts: UartChipView[]
}

export interface BenchView {
  mcus: McuStripView[]
  durationS: number
  /** The recorder hit its cap: waveforms and log are cut short. */
  truncated: boolean
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

/** Core states that are trouble, and how bad. */
function coreTone(core: McuBench['core']): BenchTone {
  if (core === 'core_trapped' || core === 'core_faulted') return 'err'
  if (core === 'core_parked') return 'warn'
  return 'ok'
}

export function pinChip(row: PinRow, mcuRef: string): PinChipView {
  const net = row.net || null
  return {
    key: `${mcuRef}:${row.pin}`,
    pin: row.pin,
    net,
    summary: row.summary,
    muted: row.state === 'pin_input',
    truncated: !!row.samples_truncated,
    title: net
      ? `${row.pin} drives ${net}: show it on the board and in the run view`
      : `${row.pin} is not wired to a net on this board, so there is nothing to show on the board`,
  }
}

export function uartChip(row: UartRow, mcuRef: string): UartChipView {
  const baud = row.baud ? `${Number(row.baud.toPrecision(6))} baud` : null
  const baudNote = baud ? null : (row.baud_note || 'baud not decoded')
  const text = row.bytes > 0 && row.printable_ratio < 0.7
    ? `only ${Math.round(row.printable_ratio * 100)}% of the bytes are text`
    : null
  return {
    key: `${mcuRef}:${row.port}`,
    port: row.port,
    net: row.net || null,
    baud,
    baudNote,
    bytes: plural(row.bytes, 'byte'),
    firstLine: row.first_line ? row.first_line : null,
    garbled: text,
    title: (row.net
      ? `${row.port} transmits on ${row.net}: show it on the board and in the run view`
      : `${row.port} is not tied to a net the binder knows, so there is nothing to show on the board`)
      + (baudNote ? ` (${baudNote})` : ''),
  }
}

export function mcuStrip(m: McuBench): McuStripView {
  const tone: BenchTone = coreTone(m.core) !== 'ok' ? coreTone(m.core) : m.resets > 0 ? 'warn' : 'ok'
  const detail: string[] = []
  if (m.park) {
    const where = `${m.park.symbol ? `${m.park.symbol}, ` : ''}pc 0x${m.park.pc.toString(16).padStart(8, '0')}`
    detail.push(where)
    if (m.park.quiet_since_s != null) detail.push(`no output since ${formatSimTime(m.park.quiet_since_s)}`)
  }
  const facts: string[] = []
  if (m.resets > 0) facts.push(plural(m.resets, 'reset'))
  facts.push(m.first_activity_s != null ? `first activity at ${formatSimTime(m.first_activity_s)}` : 'no activity')
  return {
    ref: m.mcu_ref,
    backend: labelFor(m.backend),
    coreLabel: labelFor(m.core),
    headline: m.headline,
    tone,
    issue: tone !== 'ok',
    detail,
    facts,
    pins: m.pins.map(p => pinChip(p, m.mcu_ref)),
    uarts: (m.uarts ?? []).map(u => uartChip(u, m.mcu_ref)),
  }
}

export function benchView(b: BenchReport): BenchView {
  return { mcus: b.mcus.map(mcuStrip), durationS: b.duration_s, truncated: !!b.truncated }
}

/** The nets a bench report ties to MCU pins (pins and UART TX lines), for the
 *  live scope to draw as lanes instead of volt traces. */
export function digitalNets(b: BenchReport | null | undefined): Set<string> {
  const out = new Set<string>()
  for (const m of b?.mcus ?? []) {
    for (const p of m.pins) if (p.net && p.state !== 'pin_input') out.add(p.net)
    for (const u of m.uarts ?? []) if (u.net) out.add(u.net)
  }
  return out
}

// ── The run view ─────────────────────────────────────────────────────────

/** How many lanes the run view draws before it folds the quiet pins away. */
export const MAX_LANES = 10

/** The pins worth a lane, loudest first: the focused one always, then toggling
 *  pins, then pins held at a level. A pin the firmware never drove has no lane. */
export function laneRows(b: BenchReport, focusNet: string | null, max = MAX_LANES): { rows: PinRow[]; folded: number } {
  const all = b.mcus.flatMap(m => m.pins).filter(p => p.state !== 'pin_input' || (p.edge_samples?.length ?? 0) > 0)
  const rank = (p: PinRow) => (focusNet && p.net === focusNet ? 0 : p.state === 'pin_toggling' ? 1 : 2)
  const sorted = all.map((p, i) => ({ p, i })).sort((a, c) => rank(a.p) - rank(c.p) || a.i - c.i)
  const kept = new Set(sorted.slice(0, max).map(x => x.p))
  // Keep the board's own order among the pins that stay.
  return { rows: all.filter(p => kept.has(p)), folded: all.length - kept.size }
}

/** A pin as a lane: its edges, the level before them, its colour slot. */
export function laneOf(row: PinRow, slot: number, focusNet: string | null): ScopeLane {
  const edges = row.edge_samples ?? []
  const held = row.final_high ?? row.state === 'pin_static_high'
  return {
    label: row.pin,
    colorToken: traceToken(slot),
    edges,
    initial: initialLevel(edges, held),
    // Cut short only when the engine says so; a count that merely differs is not a claim.
    totalEdges: row.samples_truncated ? Math.max(row.edges, edges.length + 1) : edges.length,
    focused: !!focusNet && row.net === focusNet,
  }
}

/** The lanes, the pin row behind each (same order), and how many were folded. */
export function benchLanes(b: BenchReport, focusNet: string | null): { lanes: ScopeLane[]; rows: PinRow[]; folded: number } {
  const { rows, folded } = laneRows(b, focusNet)
  return { lanes: rows.map((r, i) => laneOf(r, i, focusNet)), rows, folded }
}

/** The bus log as boxes for the decoded track. */
export function decodedItems(log: BusTransaction[] | undefined): DecodedItem[] {
  return (log ?? []).map(t => ({ t: t.t_s, summary: t.summary, err: !t.acked }))
}

/** How loud an event's rule is: trouble, a warning, or just information. */
export function eventTone(kind: BenchEvent['kind']): 'err' | 'warn' | 'note' {
  if (kind === 'event_fault') return 'err'
  if (kind === 'event_first_edge' || kind === 'event_first_uart_byte') return 'note'
  return 'warn'
}

/** The events as labelled rules. The label is the vocabulary's word for the kind;
 *  the engine's own sentence about it is the tooltip's business. */
export function eventMarkers(events: BenchEvent[] | undefined): { x: number; label: string; tone: 'err' | 'warn' | 'note' }[] {
  return (events ?? []).map(e => ({ x: e.t_s, label: labelFor(e.kind), tone: eventTone(e.kind) }))
}

/** A sentence for the run view's accessible name. */
export function benchSummaryLine(b: BenchReport, laneCount: number): string {
  const parts = [`${plural(laneCount, 'pin lane')} over ${formatSimTime(b.duration_s)} of simulated time`]
  if (b.bus_log?.length) parts.push(plural(b.bus_log.length, 'decoded bus transaction'))
  if (b.events?.length) parts.push(plural(b.events.length, 'event marker'))
  return parts.join(', ')
}

// ── The live scope ───────────────────────────────────────────────────────

/** A probed net as a lane, from the voltages the live sim measured: a sample
 *  above half the supply the pin swings to is high. The supply is the larger of
 *  3.3 V and the highest level seen, so a 5 V part and a 1.8 V part both read
 *  sensibly; only the crossings are kept. */
export function liveLane(label: string, slot: number, samples: { t: number; v: number }[]): ScopeLane {
  const top = Math.max(3.3, ...samples.map(s => s.v))
  const threshold = top / 2
  const edges: ScopeLane['edges'] = []
  let level: boolean | null = null
  let initial = false
  for (const s of samples) {
    const high = s.v > threshold
    if (level === null) initial = high
    else if (high !== level) edges.push({ t_s: s.t, high })
    level = high
  }
  return { label: label.length > 12 ? `${label.slice(0, 11)}\u2026` : label, colorToken: traceToken(slot), edges, initial }
}
