// Findings grouping, shared by the on-screen report and the exported one.
//
// This lived inside BoardView until the HTML export needed the same collapse.
// Two copies of it would have been two reports: the page saying "50 similar
// findings, same cause" and the downloaded file listing fifty cards, from the
// same JSON. The export is meant to BE the report, so the grouping is one
// function.

import type { WebFinding } from '../types/report'

/** A run of findings that share level + why + fix (same-shaped): the DRC
 *  clearance case where 128 warnings differ only in which net-pair/location.
 *  Each item keeps its own `what` AND its own board location (if any). */
export interface FindingGroup {
  level: string
  why: string
  fix: string
  items: { what: string; x?: number; y?: number }[]
}

/** Collapse same-shaped findings so the shared explanation is shown ONCE.
 *  Order-independent: any findings with identical level/why/fix merge, no
 *  matter where they sit in the list. Nothing is hidden; every individual
 *  `what` is still listed, just under one explanation. */
export function groupFindings(findings: WebFinding[]): FindingGroup[] {
  const groups: FindingGroup[] = []
  for (const f of findings) {
    const item = { what: f.what, x: f.x, y: f.y }
    const g = groups.find(x => x.level === f.level && x.why === f.why && x.fix === f.fix)
    if (g) g.items.push(item)
    else groups.push({ level: f.level, why: f.why, fix: f.fix, items: [item] })
  }
  return groups
}
