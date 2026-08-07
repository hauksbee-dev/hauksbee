import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { refusalLines, type RefusalContract } from '../src/lib/refusal-contract'

describe('C5.3 refusal contract', () => {
  test('the browser preserves all four answers from the CI result', () => {
    const refusal: RefusalContract = {
      claim: 'overall CI verdict for boot checks',
      missing_prerequisite: 'a converged analog solve across the assertion window',
      valid_partial_conclusions: ['the UART assertion passed', 'the spec and board loaded'],
      next_action: 'fix the first named failed window, then rerun the same spec',
    }

    expect(refusalLines(refusal)).toEqual([
      ['Refused claim', 'overall CI verdict for boot checks'],
      ['Missing prerequisite', 'a converged analog solve across the assertion window'],
      ['Still valid', 'the UART assertion passed; the spec and board loaded'],
      ['Next action', 'fix the first named failed window, then rerun the same spec'],
    ])
  })

  test('both analysis and checks views render the shared contract', () => {
    const boardView = readFileSync(new URL('../src/components/BoardView.tsx', import.meta.url), 'utf8')
    const checksView = readFileSync(new URL('../src/components/ChecksView.tsx', import.meta.url), 'utf8')

    expect(boardView).toContain("from '../lib/refusal-contract'")
    expect(boardView).toContain('refusalLines(r.refusal)')
    expect(boardView).toContain('data-testid="analysis-refusal-contract"')
    expect(checksView).toContain('refusalLines(result.refusal)')
  })
})
