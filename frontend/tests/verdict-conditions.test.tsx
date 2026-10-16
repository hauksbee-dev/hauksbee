// The conditions of a conditional pass, listed under the verdict. The fixtures
// are real engine output (tests/visual-lint/fixtures, captured from
// `hauksbee serve` on the pMOS switch boards): a pass that rests on assumed
// corners carries `verdict_conditions`; a fail and an unconditional pass carry
// none and must show nothing extra. A partly judged run (`partly_judged` with
// its `judgement_scope`) is a non-pass of its own, naming what was judged.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { ShellChips } from '../src/components/ShellChips'
import { PartlyJudged, VerdictConditions } from '../src/components/report/VerdictConditions'
import { buildReportHtml } from '../src/lib/report-export'
import { explainReport, reportView, unjudgedGroups } from '../src/lib/report-view'
import type { WebReport } from '../src/types/report'

const fixture = (name: string): WebReport =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport

const conditional = fixture('analyze-conditional-pass.json')
const failing = fixture('analyze-conditional-fail.json')
const unconditional = fixture('analyze-watchy.json')
const partly = fixture('analyze-partly-judged.json')

/** The result panel's source from its verdict section to its action bar. */
const resultCard = () => {
  const panel = readFileSync(new URL('../src/components/report/ResultPanel.tsx', import.meta.url), 'utf8')
  return panel.slice(panel.indexOf('data-testid="report-verdict"'), panel.indexOf('data-testid="result-actions"'))
}

/** The names a rendered chip list shows, in order (each chip is a button). */
const chipNames = (html: string) => [...html.matchAll(/data-testid="result-link"[^>]*data-name="([^"]+)"/g)].map(m => m[1])

const CONDITION = 'Q1 passes across assumed corners (generic_pmos_sot_corner_high_vth, generic_pmos_sot_corner_low_vth); put the MPN on Q1 to remove this assumption.'

const exportInput = (report: WebReport) => ({
  report,
  boardLabel: null,
  firmwareName: null,
  analyzedAt: Date.UTC(2026, 9, 10, 12, 0, 0),
  engineVersion: '0.1.0',
  appVersion: '0.1.0',
  spec: null,
  checks: null,
  sessionName: null,
  restored: false,
})

describe('verdict conditions', () => {
  test('the conditional fixture carries the engine sentence and the headline that says it', () => {
    expect(conditional.verdict_conditions).toEqual([CONDITION])
    expect(conditional.verdict).toBe('pass')
    expect(conditional.headline).toBe(
      `OK to order, conditionally: the static checks found nothing that blocks ordering; this rests on: ${CONDITION}`,
    )
  })

  test('each condition is its own list item, with the sentence verbatim', () => {
    const two = [CONDITION, 'U2 passes across assumed corners (a, b); put the MPN on U2 to remove this assumption.']
    const html = renderToStaticMarkup(<VerdictConditions conditions={two} />)
    expect(html).toContain('data-testid="report-verdict-conditions"')
    expect(html.match(/data-testid="report-verdict-condition"/g)).toHaveLength(2)
    expect(html).toContain(CONDITION)
    expect(html).toContain('U2 passes across assumed corners')
    expect(html).toContain('Conditions this pass rests on')
    // Quiet, not a failure: the note accent, never the error palette.
    expect(html).toContain('var(--note-accent)')
    expect(html).not.toContain('--err')
  })

  test('the fixture report projects to one condition and a lead-in headline that does not repeat it', () => {
    const view = reportView(conditional)
    expect(view.conditions).toEqual([CONDITION])
    expect(view.headline).toBe(
      'OK to order, conditionally: the static checks found nothing that blocks ordering; this rests on:',
    )
    expect(view.tone).not.toBe('error')
    const html = renderToStaticMarkup(<VerdictConditions conditions={view.conditions} />)
    expect(html.split(CONDITION)).toHaveLength(2)
  })

  test('an unconditional or failing report shows nothing extra', () => {
    for (const report of [unconditional, failing]) {
      const view = reportView(report)
      expect(view.conditions).toEqual([])
      expect(view.headline).toBe(report.headline)
      expect(renderToStaticMarkup(<VerdictConditions conditions={view.conditions} />)).toBe('')
    }
    expect(reportView({ ...unconditional, verdict_conditions: [] }).conditions).toEqual([])
    expect(reportView({ ...unconditional, verdict_conditions: ['', '  '] }).conditions).toEqual([])
  })

  test('a headline that is not the conditional one is left alone', () => {
    const view = reportView({ ...unconditional, verdict_conditions: [CONDITION] })
    expect(view.headline).toBe(unconditional.headline)
    expect(view.conditions).toEqual([CONDITION])
  })

  test('the analysis view mounts the list inside the result panel, above its action bar', () => {
    expect(resultCard()).toContain('<VerdictConditions conditions={v.conditions} />')
    expect(readFileSync(new URL('../src/components/BoardView.tsx', import.meta.url), 'utf8')).toContain('<ResultPanel')
  })

  test('the exported report lists the conditions and a clean one lists none', () => {
    const html = buildReportHtml(exportInput(conditional))
    expect(html).toContain(CONDITION)
    expect(html.split(CONDITION)).toHaveLength(2)
    expect(buildReportHtml(exportInput(unconditional))).not.toContain('assumed corners')
  })
})

describe('partly judged', () => {
  test('the fixture is the engine report of a run with an unmodelled key part', () => {
    expect(partly.verdict).toBe('invalid')
    expect(partly.partly_judged).toBe(true)
    expect(partly.judgement_scope?.open_parts).toEqual(['Q1'])
    expect(partly.headline).toMatch(/^Partly judged, not a pass/)
    expect(partly.verdict_reason).toMatch(/^PARTLY JUDGED \(not a pass\)/)
    expect(partly.verdict_conditions ?? []).toEqual([])
  })

  test('the view carries the judged scope, from the same numbers the engine states', () => {
    const scope = partly.judgement_scope!
    const view = reportView(partly)
    expect(view.partlyJudged).toEqual({
      openParts: scope.open_parts,
      wiring: scope.wiring_dependent,
      electrical: scope.electrical_dependent,
      judged: 2 * scope.nets - scope.wiring_dependent.length - scope.electrical_dependent.length,
      total: 2 * scope.nets,
    })
    expect(view.tone).toBe('warning')
    expect(view.conditions).toEqual([])
    expect(explainReport(partly)).toBeNull()
  })

  test('only a partly judged report has a scope', () => {
    for (const report of [conditional, failing, unconditional]) expect(reportView(report).partlyJudged).toBeNull()
    expect(renderToStaticMarkup(<PartlyJudged scope={null} />)).toBe('')
  })

  test('the block says not a pass, names the part, what was not judged and what was', () => {
    const html = renderToStaticMarkup(<PartlyJudged scope={reportView(partly).partlyJudged} />)
    expect(html).toContain('data-testid="report-partly-judged"')
    expect(html).toContain('Not a pass: partly judged')
    expect(html).toContain('No usable model: </span>')
    expect(chipNames(html.slice(html.indexOf('partly-judged-open')))[0]).toBe('Q1')
    expect(html).toContain('electrical checks')
    // One list under the one heading: every net named once, however many kinds.
    expect(html.match(/Not judged on/g)).toHaveLength(1)
    expect(html).not.toContain('partly-judged-wiring')
    expect(html).not.toContain('partly-judged-electrical')
    const names = chipNames(html.slice(html.indexOf('partly-judged-unjudged'), html.indexOf('partly-judged-judged')))
    expect(new Set(names).size).toBe(names.length)
    expect(html).toMatch(/Judged: <\/span>\d+ of \d+ net checks/)
    // A warning, not the quiet note accent of a conditional pass and not an error.
    expect(html).toContain('var(--warn)')
    expect(html).not.toContain('--err')
    expect(html).not.toContain('--note-accent')
  })

  test('a long net list is cut to a few names, with the whole list in the tooltip', () => {
    const nets = Array.from({ length: 9 }, (_, i) => `N${i}`)
    const html = renderToStaticMarkup(
      <PartlyJudged scope={{ openParts: ['U1', 'U2'], wiring: ['A'], electrical: nets, judged: 4, total: 20 }} />,
    )
    const electrical = html.slice(html.indexOf('partly-judged-kind-electrical'), html.indexOf('partly-judged-kind-wiring'))
    expect(chipNames(electrical)).toEqual(['N0', 'N1', 'N2', 'N3', 'N4', 'N5'])
    expect(electrical).toContain('and 3 more')
    expect(electrical).toContain('title="N0, N1, N2, N3, N4, N5, N6, N7, N8"')
    expect(html).toContain('Not judged on 10 nets, by kind:')
    expect(html).toContain('wiring and value checks, 1 net:')
    expect(html).toContain('electrical checks (levels, ratings, rails), 9 nets:')
    expect(html).toContain('give U1, U2 models.')
  })

  test('a net in both sets is named once, under the kind that covers both', () => {
    expect(unjudgedGroups({ wiring: ['A', 'B'], electrical: ['A', 'B', 'C'] })).toEqual([
      { kind: 'both', label: 'wiring, value and electrical checks', nets: ['A', 'B'] },
      { kind: 'electrical', label: 'electrical checks (levels, ratings, rails)', nets: ['C'] },
    ])
    // Control: disjoint sets stay in their own kinds; empty sets give no rows.
    expect(unjudgedGroups({ wiring: ['W'], electrical: ['E'] }).map(g => g.kind)).toEqual(['electrical', 'wiring'])
    expect(unjudgedGroups({ wiring: [], electrical: [] })).toEqual([])
  })

  test('an older engine that sends only the flag still gets the non-pass block', () => {
    const view = reportView({ ...partly, judgement_scope: undefined })
    expect(view.partlyJudged).toEqual({ openParts: [], wiring: [], electrical: [], judged: 0, total: 0 })
    const html = renderToStaticMarkup(<PartlyJudged scope={view.partlyJudged} />)
    expect(html).toContain('Not a pass: partly judged')
  })

  test('the analysis view mounts the block inside the verdict card and drops the clipped reason', () => {
    const card = resultCard()
    expect(card).toContain('<PartlyJudged scope={v.partlyJudged} />')
    expect(card).toContain('v.reason && !v.partlyJudged')
  })

  test('the exported report keeps the full partly judged sentence', () => {
    const html = buildReportHtml(exportInput(partly))
    expect(html).toContain('PARTLY JUDGED (not a pass)')
    expect(html).toContain('Q1 has no usable model')
  })
})

describe('the header chip', () => {
  const noop = () => {}
  const chip = (report: WebReport) =>
    renderToStaticMarkup(
      <ShellChips
        view="board"
        report={report}
        checks={null}
        sim={{ running: false, faults: 0, connected: false, sessionBoard: null } as never}
        simMounted={false}
        simOffline={false}
        sessionBoard={null}
        sessionMatchesCurrent={false}
        currentBoard={null}
        serverLiveActive={false}
        goTo={noop}
        onOpenLiveSession={noop}
        onDriveLive={noop}
      />,
    )

  test('a conditional pass is not a plain PASS and a partly judged run is INVALID, not a pass', () => {
    expect(chip(conditional)).toContain('Conditional pass')
    expect(chip(unconditional)).not.toContain('Conditional')
    expect(chip(unconditional)).toContain('PASS')
    expect(chip(partly)).toContain('INVALID')
    expect(chip(partly)).toContain('partly judged, not a pass')
    expect(chip(partly)).not.toContain('undermined')
  })

  test('is ONE chip: board and checks fold into the worst claim, listed in its hover', () => {
    const html = renderToStaticMarkup(
      <ShellChips
        view="board"
        report={unconditional}
        checks={{ passed: 3, failed: 1, invalid: 0, measured: 0 } as never}
        sim={{ running: false, faults: 0, connected: false, sessionBoard: null } as never}
        simMounted={false}
        simOffline={false}
        sessionBoard={null}
        sessionMatchesCurrent={false}
        currentBoard={null}
        serverLiveActive={false}
        goTo={noop}
        onOpenLiveSession={noop}
        onDriveLive={noop}
      />,
    )
    expect(html.match(/<button/g)?.length).toBe(1)
    expect(html).toContain('FAIL')
    expect(html).toContain('1 check failed')
    expect(html).toContain('Board: PASS')
  })

  test('Environment and Settings carry no board chip', () => {
    for (const view of ['env', 'settings'] as const) {
      const html = renderToStaticMarkup(
        <ShellChips
          view={view}
          report={unconditional}
          checks={null}
          sim={{ running: false, faults: 0, connected: false, sessionBoard: null } as never}
          simMounted={false}
          simOffline={false}
          sessionBoard={null}
          sessionMatchesCurrent={false}
          currentBoard={null}
          serverLiveActive={false}
          goTo={noop}
          onOpenLiveSession={noop}
          onDriveLive={noop}
        />,
      )
      expect(html).toBe('')
    }
  })
})
