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
import type { WebReport } from '../types/report'
import { plural, reportVerdictTone, summarizeEvidence } from './report-view'

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

/** The pill colours of each tone, shared by every surface that shows a verdict. */
export const TONE_STYLE: Record<ChipTone, React.CSSProperties> = {
  ok: { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' },
  err: { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err)' },
  warn: { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--warn)' },
  quiet: { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' },
}

/** Severity order used to pick the one chip a header shows. */
export const TONE_RANK: Record<ChipTone, number> = { err: 3, warn: 2, ok: 1, quiet: 0 }

/** The board verdict of an analyzed report as a glossary term, a short detail
 *  and a tone: the one derivation behind the header chip and a saved session's
 *  verdict. `null` when the run did not produce a report. */
export function boardVerdict(report: WebReport): { term: StatusTerm; detail: string; tone: ChipTone } | null {
  if (report.ok !== true) return null
  const tone = reportVerdictTone(report)
  const conditional = report.verdict === 'pass' && (report.verdict_conditions ?? []).some(c => c.trim() !== '')
  let board: { term: StatusTerm; detail: string; tone?: ChipTone }
  if (tone === 'error') {
    // The not-routed state is itself one of the counted serious findings, so a
    // board with no other serious finding says what that one is.
    board = report.fix_first?.notice && report.serious <= 1
      ? { term: 'fail', detail: 'not routed yet' }
      : report.serious > 0
        ? { term: 'fail', detail: `${report.serious} serious` }
        : { term: 'fail', detail: 'analysis failed' }
  } else if (tone === 'warning') {
    // An undermined run-level claim is what the body headline leads with.
    board = report.partly_judged
      ? { term: 'invalid', detail: 'partly judged, not a pass' }
      : summarizeEvidence(report.evidence).undermined > 0
        ? { term: 'invalid', detail: 'evidence undermined' }
        : conditional
          ? { term: 'conditional_pass', detail: report.total > 0 ? plural(report.total, 'finding') : 'rests on conditions' }
          // Findings without conditions are qualifications on a pass, not a conditional pass.
          : { term: 'pass', detail: report.total > 0 ? plural(report.total, 'finding') : 'analysis qualified', tone: 'warn' }
  } else {
    // A pass that rests on conditions is never the plain PASS.
    board = conditional ? { term: 'conditional_pass', detail: 'rests on conditions' } : { term: 'pass', detail: '' }
  }
  return { ...board, tone: board.tone ?? TERM_TONE[board.term] }
}
