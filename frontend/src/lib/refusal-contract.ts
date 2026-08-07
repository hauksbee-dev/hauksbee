/** The C5.3 contract emitted by every invalid-for-analysis result. */
export interface RefusalContract {
  claim: string
  missing_prerequisite: string
  valid_partial_conclusions: string[]
  next_action: string
}

/** Lossless display rows; keeping this pure makes field loss testable. */
export function refusalLines(refusal: RefusalContract): [string, string][] {
  return [
    ['Refused claim', refusal.claim],
    ['Missing prerequisite', refusal.missing_prerequisite],
    ['Still valid', refusal.valid_partial_conclusions.join('; ')],
    ['Next action', refusal.next_action],
  ]
}
