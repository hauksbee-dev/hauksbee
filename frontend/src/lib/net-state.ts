// What a net's number MEANS, in one place.
//
// A voltage of 0.000 V means one of three different things, and the reader
// cannot tell them apart from the number alone:
//
//   1. driven low:        a real measurement, the net is held at 0 V
//   2. not observed:      the backend cannot see this pin's drive at all, so
//                          the number is the passive network's idle level and
//                          is not a measurement of anything the MCU did
//   3. moving too fast:   the net IS being driven, but the excursion happened
//                          between samples, so the sampled instant is a true
//                          reading of a moment that is not representative
//
// This module gives every surface that prints a net one shared answer, so the
// board map, the net list, the selection card and the scope cannot disagree.
//
// `unobserved_drive_nets` is on the wire, so case 2 is answerable today. Case 3
// needs the per-net min/max WITHIN a chunk; the engine tracks it
// (`Scheduler::frame_v_extremes`) but `SimFrame` does not carry it yet, so a
// sub-chunk pulse is gone before the frame is serialised. `envelope` reads the
// engine's extremes WHEN PRESENT (the type is declared so adding the field is
// the only change needed) and otherwise falls back to the spread across the
// frames this client received — which catches a net moving between frames but
// NOT one moving within a chunk. `envelopeSource` says which of the two you are
// looking at, so the UI never implies more resolution than it has.

import type { SimFrame } from '../types/protocol'

/** Volts of movement below which a net is flat, not "excursing". Matches the
 *  renderer's own tint threshold so the two agree about a static rail. */
const MOVEMENT_FLOOR_V = 0.05

export type NetReading =
  /** The backend cannot see this pin's drive; the number is not a measurement. */
  | { kind: 'unobserved' }
  /** A measured level. `min`/`max` describe movement over the observed span. */
  | { kind: 'measured'; volts: number; min: number; max: number; moving: boolean }
  /** The frame carries no value for this net. */
  | { kind: 'absent' }

/** Per-net min/max over the frames a client retained. Not a substitute for the
 *  engine's intra-chunk extremes; see the module note. */
export type Envelopes = Map<string, [number, number]>

/** Build the fallback envelope from retained frames. Cheap and bounded: the
 *  most recent `window` frames only, so one old glitch does not make every net
 *  look busy forever. */
export function envelopesFromHistory(frames: SimFrame[], window = 30): Envelopes {
  const out: Envelopes = new Map()
  const from = Math.max(0, frames.length - window)
  for (let i = from; i < frames.length; i++) {
    const v = frames[i]?.net_voltages
    if (!v) continue
    for (const net in v) {
      const val = v[net]
      const cur = out.get(net)
      if (!cur) out.set(net, [val, val])
      else {
        if (val < cur[0]) cur[0] = val
        if (val > cur[1]) cur[1] = val
      }
    }
  }
  return out
}

/** Where an envelope came from, so the UI can be honest about its resolution. */
export type EnvelopeSource = 'engine' | 'frames' | 'none'

export function envelopeSource(frame: SimFrame | null, fallback?: Envelopes): EnvelopeSource {
  if (frame?.net_v_extremes) return 'engine'
  if (fallback && fallback.size > 0) return 'frames'
  return 'none'
}

/** The one answer about a net. */
export function readNet(
  frame: SimFrame | null,
  net: string,
  fallback?: Envelopes,
): NetReading {
  if (!frame) return { kind: 'absent' }
  if (frame.unobserved_drive_nets?.includes(net)) return { kind: 'unobserved' }
  const volts = frame.net_voltages?.[net]
  if (volts === undefined) return { kind: 'absent' }

  // The engine's intra-chunk extremes when the wire carries them, else the
  // spread across retained frames, else no movement information at all.
  const eng = frame.net_v_extremes?.[net]
  const span = eng ?? fallback?.get(net)
  const min = span ? Math.min(span[0], volts) : volts
  const max = span ? Math.max(span[1], volts) : volts
  return { kind: 'measured', volts, min, max, moving: max - min > MOVEMENT_FLOOR_V }
}

/** The readout string. Never prints a confident number for a net nobody
 *  measured, and never hides that a net is swinging. */
export function netReadoutText(r: NetReading): string {
  switch (r.kind) {
    case 'unobserved': return 'not observed'
    case 'absent': return '—'
    case 'measured':
      return r.moving
        ? `${r.min.toFixed(2)}–${r.max.toFixed(2)} V`
        : `${r.volts.toFixed(3)} V`
  }
}
