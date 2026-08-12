import type { WebReport } from '../types/report'
import { summarizeEvidence } from './evidence'

export type ReportVerdictTone = 'ok' | 'warning' | 'error'

/** One verdict contract for the browser card and the standalone export. */
export function reportVerdictTone(report: WebReport): ReportVerdictTone {
  if (
    report.serious > 0
    || (report.cosim?.findings ?? []).some(finding => finding.level === 'serious')
  ) return 'error'

  const bindOpen = !!report.bind?.active_path_unresolved?.length
  const hasHeadsUp = (report.sections ?? []).some(section => section.heads_up?.length)
  const cosimQualified = !!(
    report.refusal
    || report.cosim?.findings?.length
    || report.cosim?.timing_refusals?.length
    || report.cosim?.fallback_windows?.length
    || report.cosim?.analog_valid === false
  )
  if (
    report.total > 0
    || bindOpen
    || hasHeadsUp
    || summarizeEvidence(report.evidence).caveated > 0
    || cosimQualified
  ) return 'warning'

  return 'ok'
}

export function reportVerdictPalette(report: WebReport): { border: string, background: string } {
  switch (reportVerdictTone(report)) {
    case 'error': return { border: 'var(--err-border)', background: 'var(--err-bg)' }
    case 'warning': return { border: 'var(--warn-border)', background: 'var(--warn-bg)' }
    case 'ok': return { border: 'var(--ok-border)', background: 'var(--ok-bg)' }
  }
}
