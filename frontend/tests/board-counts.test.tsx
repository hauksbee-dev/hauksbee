// The Board view states the parts with no usable model once, as one measure,
// and names a run that is neither a pass nor a fail with the shared
// vocabulary's word for its state (the CLI's word), never the check-level
// INVALID. While an inspector card is open its own button is the screen's one
// primary, so the Next step's button steps down.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { NextStep } from '../src/components/report/NextStep'
import { FindingsList } from '../src/components/report/Findings'
import { PartlyJudged } from '../src/components/report/VerdictConditions'
import { VerdictPill } from '../src/components/VerdictPill'
import { findingEntries, reportView, sectionStatus, unmodelledHome, unmodelledSentence } from '../src/lib/report-view'
import { boardStateWords, boardVerdict, savedVerdict } from '../src/lib/status-vocab'
import { LABEL_ENTRIES } from '../src/lib/labels'
import type { WebReport } from '../src/types/report'

const fixture = (name: string): WebReport =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as WebReport

const partly = fixture('analyze-partly-judged.json')
const unrouted = fixture('analyze-unrouted.json')
const notConclusive = fixture('analyze-watchy.json')

describe('one measure of the parts with no usable model', () => {
  test('a partly judged run states it in the verdict card and nowhere beside the board', () => {
    const v = reportView(partly)
    expect(unmodelledHome(partly, v.unmodelled)).toBe('verdict')
    const row = renderToStaticMarkup(<PartlyJudged scope={v.partlyJudged} unmodelled={v.unmodelled} />)
    expect(row).toContain('(key part) has no usable model.')
    const list = renderToStaticMarkup(
      <FindingsList entries={findingEntries(v)} status={sectionStatus(v)} openParts={null} openRefs={new Set()} preLayout={v.preLayout} />,
    )
    expect(list).not.toContain('open-parts-banner')
    expect(list).not.toContain('no usable model')
  })

  test('a failing board with unmodelled key parts states it once, as a finding, with every action', () => {
    const v = reportView(unrouted)
    expect(unmodelledHome(unrouted, v.unmodelled)).toBe('findings')
    const list = renderToStaticMarkup(
      <FindingsList entries={findingEntries(v)} status={sectionStatus(v)} openParts={v.unmodelled} openRefs={new Set(v.unmodelled.all)} preLayout={v.preLayout} onListParts={() => {}} />,
    )
    expect(list.match(/data-testid="open-parts-banner"/g)).toHaveLength(1)
    expect(list).toContain(`data-open-count="${v.unmodelled.all.length}"`)
    const tail = unmodelledSentence(v.unmodelled).slice(unmodelledSentence(v.unmodelled).indexOf('('))
    expect(list).toContain(tail)
    expect(list).toContain(`List all ${v.unmodelled.all.length}`)
  })
})

describe('the word for a run that is neither a pass nor a fail', () => {
  test('comes from the shared verdict states, the CLI words', () => {
    const verdicts = Object.fromEntries(LABEL_ENTRIES.filter(e => e.group === 'verdict').map(e => [e.id, e.label]))
    expect(verdicts.partly_judged).toBe('partly judged, not a pass')
    expect(verdicts.invalid_run).toBe('inconclusive, not a pass')
    expect(boardStateWords('partly_judged')).toEqual({ word: 'PARTLY JUDGED', detail: 'not a pass' })
    expect(boardStateWords('invalid_run')).toEqual({ word: 'INCONCLUSIVE', detail: 'not a pass' })
  })

  test('partly judged and not conclusive runs each get their own state, never PASS', () => {
    expect(boardVerdict(partly)).toMatchObject({ term: 'partly_judged', tone: 'warn' })
    expect(boardVerdict(notConclusive)).toMatchObject({ term: 'invalid_run', detail: 'not a pass', tone: 'warn' })
  })

  test('a session saved under the old INVALID reads as the state it was', () => {
    const old: NonNullable<ReturnType<typeof boardVerdict>> = { term: 'invalid', detail: 'partly judged, not a pass', tone: 'warn' }
    expect(savedVerdict(old)).toEqual({ term: 'partly_judged', detail: 'not a pass', tone: 'warn' })
    const html = renderToStaticMarkup(<VerdictPill verdict={old} />)
    expect(html).toContain('PARTLY JUDGED · not a pass')
    expect(html).not.toContain('INVALID')
  })
})

describe('one primary button while the inspector is open', () => {
  const step = (quiet: boolean) => renderToStaticMarkup(
    <NextStep
      report={unrouted} view={reportView(unrouted)} firmwareName={null} checks={null} specFile={null}
      onQueue={() => {}} onOpenChecks={() => {}} host={{}} quiet={quiet}
    />,
  )
  test('the routed-board button is primary on its own and secondary beside an open card', () => {
    expect(step(false)).toContain('hb-btn-primary')
    expect(step(true)).not.toContain('hb-btn-primary')
    expect(step(true)).toContain('Drop the routed board')
  })
  test('so is the model button of a partly judged board', () => {
    // The button is the engine's typed action (the fixture predates `next_actions`).
    const withActions: WebReport = {
      ...partly,
      next_actions: [{ id: 'model_parts', label: 'Give Q1 a model', args: { refs: ['Q1'] } }],
    }
    const html = renderToStaticMarkup(
      <NextStep report={withActions} view={reportView(withActions)} firmwareName={null} checks={null} specFile={null}
        onQueue={() => {}} onOpenChecks={() => {}} host={{ giveModel: () => {} }} quiet />,
    )
    expect(html).toContain('Give Q1 a model')
    expect(html).not.toContain('hb-btn-primary')
  })
})
