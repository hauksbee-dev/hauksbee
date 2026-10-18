// The board result as something to take away and use formally: the CI's own
// verdict document, a Markdown summary for a pull request or an issue, and the
// command that reproduces the run.
//
// The JSON is the `run --check --json` document. A current server sends it
// with the report (`check_report`, rendered by the CLI's own serializer), so it
// is passed through untouched. A report from an older engine, or a session
// saved before that field existed, gets the verdict fields rebuilt from the
// report itself, and says so in `export_note`, because a rebuilt document has
// no DRC/AC/thermal sections and must not pass for the CLI's.

import type { CheckReportFinding, CheckReportJson, WebReport } from '../types/report'
import { entitiesIn, vocabulary } from './result-links'
import { boardNames, withLabels } from './plain-ids'
import { plural, reportView, unjudgedGroups, unmodelledSentence } from './report-view'

export const REBUILT_NOTE =
  'Rebuilt in the browser from the board report (this engine did not send its run --check --json document): '
  + 'the verdict, reason, conditions, judged scope and findings are the engine\'s; the per-check sections are not '
  + 'included. Run the reproduce command for the full document.'

export const OMITTED_NOTE =
  'This board is large, so the server left its full run --check --json document out of the page '
  + '(it repeats the findings, evidence and copper detail shown here). The verdict, reason, conditions, '
  + 'judged scope and findings below are the engine\'s; run the reproduce command for the full document.'

/** The CLI's verdict word for a report, as `VERDICT: <tag>:` prints it. */
export function verdictTag(r: Pick<WebReport, 'verdict' | 'partly_judged' | 'verdict_conditions' | 'verdict_inspect' | 'serious'>): string {
  const verdict = r.verdict ?? (r.serious > 0 ? 'fail' : undefined)
  if (verdict === 'pass') {
    // A pass with items to inspect is tagged in the engine's words, never "clean".
    const tag = r.verdict_inspect?.trim() || 'clean'
    return (r.verdict_conditions ?? []).some(c => c.trim()) ? `${tag}, conditionally` : tag
  }
  if (verdict === 'fail') return 'failing'
  if (r.partly_judged) return 'PARTLY JUDGED'
  return 'inconclusive'
}

function rebuiltFindings(r: WebReport): CheckReportFinding[] {
  const vocab = vocabulary(r)
  return (r.sections ?? []).flatMap(section => section.findings.map(f => {
    const named = entitiesIn(f.what, vocab)
    return {
      check: section.title,
      kind: 'finding',
      severity: f.level,
      nets: named.flatMap(e => (e.kind === 'net' ? [e.net] : [])),
      location_mm: f.x !== undefined && f.y !== undefined ? [f.x, f.y] as [number, number] : null,
      refs: named.flatMap(e => (e.kind === 'part' ? [e.ref] : [])),
      actionable: f.level === 'serious' || f.level === 'warning',
      message: f.what,
      plain: f.what,
      fix: f.fix || null,
    }
  }))
}

/** The `run --check --json` document for this report, and whether it is the
 *  engine's own (`exact`) or rebuilt from the report. */
export function checkReportDocument(r: WebReport): { doc: CheckReportJson | Record<string, unknown>; exact: boolean } {
  if (r.check_report) return { doc: r.check_report, exact: true }
  const verdict = r.verdict ?? (r.serious > 0 ? 'fail' : 'invalid')
  const conditions = (r.verdict_conditions ?? []).filter(c => c.trim())
  const doc: Record<string, unknown> = {
    ok: verdict === 'pass',
    verdict,
    ...(r.partly_judged ? { partly_judged: true } : {}),
    ...(r.verdict_reason ? { verdict_reason: r.verdict_reason } : {}),
    serious_count: r.serious,
    actionable_count: r.total,
    export_note: r.check_report_omitted_bytes ? OMITTED_NOTE : REBUILT_NOTE,
    board: r.board_name || r.file_name,
    ...(r.inventory?.length ? { inventory: r.inventory } : {}),
    ...(r.assumptions?.length ? { assumptions: r.assumptions } : {}),
    ...(r.evidence?.length ? { evidence: r.evidence } : {}),
    ...(r.refusal ? { refusal: r.refusal } : {}),
    findings: rebuiltFindings(r),
    ...(r.notes?.length ? { notes: r.notes } : {}),
    ...(verdict === 'pass' && conditions.length ? { verdict_conditions: conditions } : {}),
    ...(r.judgement_scope ? { judgement_scope: r.judgement_scope } : {}),
  }
  return { doc, exact: false }
}

export const checkReportJson = (r: WebReport): string => `${JSON.stringify(checkReportDocument(r).doc, null, 2)}\n`

/** Shell-quote a path only when it needs it. */
const arg = (s: string) => (/^[\w@%+=:,./-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`)

/** The command that reproduces this run from a terminal. */
export function cliCommand({ board, firmware, json = false }: { board: string; firmware?: string | null; json?: boolean }): string {
  return [
    'hauksbee run', arg(board), '--check',
    ...(firmware ? ['--firmware', arg(firmware)] : []),
    ...(json ? ['--json'] : []),
  ].join(' ')
}

const md = (s: string) => s.replace(/([\\`*_[\]<>|])/g, '\\$1').replace(/\s*\n\s*/g, ' ')

export interface MarkdownInput {
  report: WebReport
  boardLabel: string | null
  firmwareName: string | null
  engineVersion: string | null
  analyzedAt: number | null
}

/** The result as Markdown for a pull request or an issue: the verdict in the
 *  CLI's words, the judged scope, the conditions, the findings, and how to
 *  reproduce it. */
export function reportMarkdown({ report: r, boardLabel, firmwareName, engineVersion, analyzedAt }: MarkdownInput): string {
  const v = reportView(r)
  const board = boardLabel ?? r.file_name
  const tag = verdictTag(r)
  const keep = boardNames(r)
  // Engine prose, with any engine id inside it said in words (the Markdown has
  // no tooltip to keep the id in), then escaped.
  const say = (s: string) => md(withLabels(s, keep))
  const lines: string[] = []
  lines.push(`## Hauksbee board result: ${md(v.title)}`, '')
  lines.push(`**VERDICT: ${tag}** · ${plural(r.serious, 'serious finding')} · ${plural(r.total, 'finding')} in all`, '')
  lines.push(`> ${say(v.headline)}`, '')
  const reason = r.verdict_why ?? r.verdict_reason
  if (reason) lines.push(`**Reason:** ${say(reason)}`, '')
  const scope = v.partlyJudged
  if (scope) {
    lines.push('### Judged scope (not a pass: partly judged)', '')
    if (scope.total > 0) lines.push(`- Judged: ${scope.judged} of ${scope.total} net checks`)
    if (v.unmodelled.all.length) lines.push(`- ${say(unmodelledSentence(v.unmodelled))}.`)
    const unjudged = unjudgedGroups(scope)
    const unjudgedCount = unjudged.reduce((n, g) => n + g.nets.length, 0)
    if (unjudgedCount) {
      lines.push(`- Not judged on ${plural(unjudgedCount, 'net')}:`)
      for (const g of unjudged) lines.push(`  - ${g.label}, ${plural(g.nets.length, 'net')}: ${g.nets.map(md).join(', ')}`)
    }
    if (v.unmodelled.key.length) lines.push(`- To judge the rest: give ${v.unmodelled.key.map(md).join(', ')} ${v.unmodelled.key.length === 1 ? 'a model' : 'models'}.`)
    lines.push('')
  }
  if (v.conditions.length) {
    lines.push(v.conditions.length === 1 ? '### Condition this pass rests on' : '### Conditions this pass rests on', '')
    for (const c of v.conditions) lines.push(`- ${say(c)}`)
    lines.push('')
  }
  const findings = v.sections.flatMap(s => s.groups.flatMap(g => g.items.map(item => ({ section: s.title, level: g.level, what: item.what, fix: g.fix }))))
  const order = (level: string) => (level === 'serious' ? 0 : level === 'warning' ? 1 : 2)
  findings.sort((a, b) => order(a.level) - order(b.level))
  lines.push('### Findings', '')
  if (findings.length === 0) lines.push('None.')
  else {
    lines.push('| Severity | Where | What | Fix |', '| --- | --- | --- | --- |')
    for (const f of findings) {
      const section = f.section.replace(/\s*\(.*\)$/, '')
      lines.push(`| ${f.level} | ${md(section)} | ${say(f.what)} | ${say(f.fix || '')} |`)
    }
  }
  lines.push('')
  lines.push('### Reproduce', '', '```sh', cliCommand({ board, firmware: firmwareName, json: true }), '```', '')
  const stamp = analyzedAt ? new Date(analyzedAt).toISOString().replace(/\.\d+Z$/, 'Z') : null
  lines.push(`<sub>${[engineVersion && `hauksbee ${md(engineVersion)}`, stamp && `analyzed ${stamp}`, md(board)].filter(Boolean).join(' · ')}</sub>`, '')
  return lines.join('\n')
}
