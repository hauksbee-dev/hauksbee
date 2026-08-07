import type { EvidenceAssumption, EvidenceMap } from '../types/report'

export interface EvidenceSummary {
  clean: number
  qualified: number
  undermined: number
  caveated: number
}

/** Count the engine's derived statuses without reinterpreting them. */
export function summarizeEvidence(maps: readonly EvidenceMap[] = []): EvidenceSummary {
  const summary: EvidenceSummary = { clean: 0, qualified: 0, undermined: 0, caveated: 0 }
  for (const map of maps) {
    summary[map.status] += 1
    if (map.status !== 'clean') summary.caveated += 1
  }
  return summary
}

/** Resolve a map's stable assumption ids through the run's canonical registry.
 * Missing ids are ignored rather than rephrased: the server owns the evidence
 * vocabulary, and a client must never fabricate a cleaner substitute. */
export function assumptionsForEvidence(
  map: EvidenceMap,
  registry: readonly EvidenceAssumption[] = [],
): EvidenceAssumption[] {
  const byId = new Map(registry.map(assumption => [assumption.id, assumption]))
  return (map.assumptions ?? []).flatMap(id => {
    const assumption = byId.get(id)
    return assumption ? [assumption] : []
  })
}
