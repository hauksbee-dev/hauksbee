import type { EvidenceAssumption, EvidenceMap, ModelOnPath } from '../types/report'

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

/** Human projection of the canonical source record. Unknown remains the word
 * unknown; the browser never substitutes a guessed percentage or range. */
export function describeModelSource(model: ModelOnPath): string {
  const accuracy = model.source.uncertainty.some(item => item.status === 'unknown')
    ? 'uncertainty unknown'
    : model.source.uncertainty.some(item =>
      item.status === 'interval' && (item.kind === 'typical-range' || item.kind === 'estimated-range'))
      ? 'non-guaranteed typical/estimated range'
      : 'validated two-sided bound'
  return `${model.reference} ${model.model_id}: ${model.source.tier} · ${model.source.validation} · ${accuracy}`
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
