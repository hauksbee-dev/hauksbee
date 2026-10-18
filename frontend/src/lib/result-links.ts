// What the board result names, as things the reader can reach: the parts and
// nets a verdict sentence mentions (so each can be a live link to the board),
// and the result laid out spatially (so the board can show it).
//
// Nothing here re-judges the board. A part or a net is only recognised when the
// report itself lists it (`components`, `nets`, the model coverage rows), and a
// finding's nets and parts come from the CLI's own machine document when the
// server sent it (`check_report.findings[*].nets / refs`), from the sentence
// otherwise.

import type { CheckReportFinding, WebReport } from '../types/report'
import type { ReportView } from './report-view'

export type ResultEntity = { kind: 'part'; ref: string } | { kind: 'net'; net: string }

/** A sentence cut into plain text and the parts/nets it names. */
export type LinkSegment = string | ResultEntity

export interface Vocabulary {
  refs: Set<string>
  nets: Set<string>
}

/** Every designator and net name the report knows about. */
export function vocabulary(r: WebReport): Vocabulary {
  const refs = new Set<string>()
  for (const c of r.components ?? []) refs.add(c.reference)
  for (const c of r.model_coverage?.components ?? []) refs.add(c.reference)
  for (const p of r.bind?.open_parts ?? []) refs.add(p.reference)
  for (const p of r.judgement_scope?.open_parts ?? []) refs.add(p)
  const nets = new Set<string>(r.nets ?? [])
  for (const n of r.judgement_scope?.wiring_dependent ?? []) nets.add(n)
  for (const n of r.judgement_scope?.electrical_dependent ?? []) nets.add(n)
  return { refs, nets }
}

/** A token can only name a part or a net in prose when it looks like one: an
 *  upper-case letter, a digit or a sign in it, and two characters at least.
 *  This keeps a net called `in` or `a` from turning ordinary words into links. */
const NAME_LIKE = /^(?=.{2,})(?=.*[A-Z0-9+]).*$/

/** Cut `text` into plain runs and the parts/nets it names, in order. A token is
 *  a run of anything but whitespace and the punctuation that ends a clause; a
 *  trailing full stop and a possessive `'s` are not part of the name. */
export function linkify(text: string, vocab: Vocabulary): LinkSegment[] {
  const out: LinkSegment[] = []
  let plain = ''
  let last = 0
  for (const m of text.matchAll(/[^\s,;:()[\]"`]+/g)) {
    const raw = m[0]
    const at = m.index ?? 0
    const name = raw.replace(/'s$/, '').replace(/[.!?]+$/, '')
    const entity: ResultEntity | null = !NAME_LIKE.test(name)
      ? null
      : vocab.refs.has(name)
        ? { kind: 'part', ref: name }
        : vocab.nets.has(name)
          ? { kind: 'net', net: name }
          : null
    if (!entity) continue
    plain += text.slice(last, at)
    if (plain) out.push(plain)
    plain = ''
    out.push(entity)
    last = at + name.length
  }
  plain += text.slice(last)
  if (plain) out.push(plain)
  return out
}

/** The parts and nets a sentence names, de-duplicated, parts first. */
export function entitiesIn(text: string, vocab: Vocabulary): ResultEntity[] {
  const seen = new Set<string>()
  const parts: ResultEntity[] = []
  const nets: ResultEntity[] = []
  for (const seg of linkify(text, vocab)) {
    if (typeof seg === 'string') continue
    const key = seg.kind === 'part' ? `p:${seg.ref}` : `n:${seg.net}`
    if (seen.has(key)) continue
    seen.add(key)
    ;(seg.kind === 'part' ? parts : nets).push(seg)
  }
  return [...parts, ...nets]
}

export type ResultLevel = 'serious' | 'warning'

const level = (severity: string): ResultLevel | null =>
  severity === 'serious' ? 'serious' : severity === 'warning' ? 'warning' : null

/** The result, laid out on the board. */
export interface ResultOverlay {
  /** Parts with no usable model: outlined, tagged "no model". */
  openRefs: string[]
  /** Nets whose checks this run did not judge: drawn hatched. */
  unjudgedNets: string[]
  /** Nets and parts a finding names, at the worst level that names them. */
  findingNets: Record<string, ResultLevel>
  findingRefs: Record<string, ResultLevel>
  /** Findings at one physical spot (board mm). */
  points: { x: number; y: number; level: ResultLevel }[]
}

const worse = (a: ResultLevel | undefined, b: ResultLevel): ResultLevel =>
  a === 'serious' || b === 'serious' ? 'serious' : 'warning'

/** The CLI's own findings, when the server sent its machine document. */
export const machineFindings = (r: WebReport): CheckReportFinding[] =>
  (r.check_report?.findings ?? []).filter(f => level(f.severity) !== null)

export function resultOverlay(r: WebReport, v: ReportView): ResultOverlay {
  const vocab = vocabulary(r)
  const openRefs = v.unmodelled.all
  const unjudgedNets = [...new Set([...(v.partlyJudged?.wiring ?? []), ...(v.partlyJudged?.electrical ?? [])])]
  const findingNets: Record<string, ResultLevel> = {}
  const findingRefs: Record<string, ResultLevel> = {}
  const points: ResultOverlay['points'] = []
  const machine = machineFindings(r)
  if (machine.length > 0) {
    for (const f of machine) {
      const lv = level(f.severity)!
      for (const n of f.nets ?? []) findingNets[n] = worse(findingNets[n], lv)
      for (const ref of f.refs ?? []) if (ref) findingRefs[ref] = worse(findingRefs[ref], lv)
      if (f.location_mm) points.push({ x: f.location_mm[0], y: f.location_mm[1], level: lv })
    }
  } else {
    for (const s of r.sections ?? []) {
      for (const f of s.findings) {
        const lv = level(f.level)
        if (!lv) continue
        for (const e of entitiesIn(f.what, vocab)) {
          if (e.kind === 'part') findingRefs[e.ref] = worse(findingRefs[e.ref], lv)
          else findingNets[e.net] = worse(findingNets[e.net], lv)
        }
        if (f.x !== undefined && f.y !== undefined) points.push({ x: f.x, y: f.y, level: lv })
      }
    }
  }
  return { openRefs, unjudgedNets, findingNets, findingRefs, points }
}

/** True when the overlay would draw something. */
export const overlayHasContent = (o: ResultOverlay): boolean =>
  o.openRefs.length + o.unjudgedNets.length + o.points.length
  + Object.keys(o.findingNets).length + Object.keys(o.findingRefs).length > 0
