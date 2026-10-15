// TEMPORARY (UX batch C, 2026-10-15): the header chip's verdict words and tones.
//
// The words are the engine glossary's own labels (`hauksbee_ir::glossary`,
// served on every report as `glossary` and read through `useTermEntry`), so the
// chip says what the verdict card and the CLI say. This module only decides
// WHICH glossary term a shell state is, its tone, and the plain fallback label
// for an engine that did not send the glossary. B1's vocabulary module in
// hauksbee-ir will serve the state-to-term mapping and tones itself; when it
// lands, delete this file and read them from there (listed for Batch D).

import { useTermEntry } from '../components/ui'

export type StatusTerm = 'pass' | 'fail' | 'invalid' | 'conditional_pass' | 'not_checked'
export type ChipTone = 'ok' | 'err' | 'warn' | 'quiet'

/** Tone of each term. */
export const TERM_TONE: Record<StatusTerm, ChipTone> = {
  pass: 'ok',
  conditional_pass: 'warn',
  invalid: 'warn',
  not_checked: 'warn',
  fail: 'err',
}

/** The words used when the engine's glossary is not there (an older server). */
const FALLBACK_LABEL: Record<StatusTerm, string> = {
  pass: 'PASS',
  fail: 'FAIL',
  invalid: 'INVALID',
  conditional_pass: 'Conditional pass',
  not_checked: 'Not checked',
}

/** The glossary label for each status term. Always the same five hooks, so the
 *  call order never changes. */
export function useStatusLabel(): (term: StatusTerm) => string {
  const rows: Record<StatusTerm, string | undefined> = {
    pass: useTermEntry('pass')?.label,
    fail: useTermEntry('fail')?.label,
    invalid: useTermEntry('invalid')?.label,
    conditional_pass: useTermEntry('conditional_pass')?.label,
    not_checked: useTermEntry('not_checked')?.label,
  }
  return term => rows[term] ?? FALLBACK_LABEL[term]
}

/** Severity order used to pick the one chip a header shows. */
export const TONE_RANK: Record<ChipTone, number> = { err: 3, warn: 2, ok: 1, quiet: 0 }
