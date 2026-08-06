import type { EvidenceMap } from '../types/report'

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
