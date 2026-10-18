// Shell and Checks follow-ups: the header chip says nothing about another live
// session on a tab with no board, the chip's tooltip opens for a pointer that
// moved (not one a reflow left under it), a not-judged run states itself once,
// the "also rests on" note is a disclosure of linked chips, and the extraction
// panel reads in plain words.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { ShellChips, createHoverIntent } from '../src/components/ShellChips'
import { AssertionGroups } from '../src/components/checks/AssertionGroups'
import { RunResults } from '../src/components/checks/RunResults'
import { VerdictBand } from '../src/components/checks/VerdictBand'
import { AssertionEvidence, ResultChip } from '../src/components/checks/pieces'
import type { RemedyActions } from '../src/components/checks/Remedies'
import { AgentActivity } from '../src/components/extract/AgentActivity'
import { applyActivityLine, attemptWords, initialActivity, type ActivityState } from '../src/components/extract/activity'
import { labelFor } from '../src/lib/labels'
import { tomlToBuilder } from '../src/lib/spec-model'
import type { EvidenceAssumption, RunResponse, WebReport } from '../src/types/report'

const noop = () => {}

// ── 1. No live-session chip on a tab with no board ──────────────────────────

const chipFor = (report: WebReport | null, view: 'board' | 'sim' = 'board') =>
  renderToStaticMarkup(
    <ShellChips
      view={view}
      report={report}
      checks={null}
      sim={{ running: false, faults: 0, connected: false, sessionBoard: null } as never}
      simMounted={false}
      simOffline={false}
      sessionBoard="boot_gate.kicad_pcb"
      sessionMatchesCurrent={false}
      currentBoard={null}
      serverLiveActive
      goTo={noop}
      onOpenLiveSession={noop}
      onDriveLive={noop}
    />,
  )

describe('the live-session chip needs a board', () => {
  test('the drop page (no report) shows no chip: the page has its own quiet line', () => {
    expect(chipFor(null)).toBe('')
  })

  test('the error page (a failed report) shows no chip either', () => {
    expect(chipFor({ ok: false, error: 'unrecognized board format' } as unknown as WebReport)).toBe('')
  })

  test('with a board loaded it still says another session is live', () => {
    const html = chipFor({ ok: true, serious: 0, total: 0 } as unknown as WebReport)
    expect(html).toContain('another session is live: boot_gate.kicad_pcb')
  })
})

// ── 2. Hover intent ─────────────────────────────────────────────────────────

describe('hover intent', () => {
  const wait = (ms: number) => new Promise(r => setTimeout(r, ms))

  test('a pointer that never moved does not open the tooltip (a reflow put the chip under it)', async () => {
    let opened = 0
    const h = createHoverIntent({ onOpen: () => { opened += 1 }, delayMs: 5 })
    h.enter(100, 20)
    await wait(30)
    h.move(100, 20)
    h.move(101, 21)
    await wait(30)
    expect(opened).toBe(0)
  })

  test('real movement inside the chip opens it after a short beat, once', async () => {
    let opened = 0
    const h = createHoverIntent({ onOpen: () => { opened += 1 }, delayMs: 10 })
    h.enter(100, 20)
    h.move(110, 24)
    h.move(112, 25)
    expect(opened).toBe(0)
    await wait(40)
    expect(opened).toBe(1)
  })

  test('leaving before the beat ends cancels it', async () => {
    let opened = 0
    const h = createHoverIntent({ onOpen: () => { opened += 1 }, delayMs: 20 })
    h.enter(100, 20)
    h.move(110, 24)
    h.leave()
    await wait(50)
    expect(opened).toBe(0)
  })
})

// ── 3 and 4. One "Not judged" display, and the linked assumptions ───────────

const assumption = (id: string, kind: string, part: string, term?: string): EvidenceAssumption => ({
  id, kind, source: 's', scope: { type: 'subjects', value: [{ kind: 'part', id: part }] }, statement: `${id} is assumed`,
  because: 'b', consequence: 'c', replacement: 'r', ...(term ? { term } : {}),
} as unknown as EvidenceAssumption)

const registry = [
  assumption('open-part:Q1', 'open_part', 'Q1'),
  assumption('open-part:U4', 'open_part', 'U4'),
  assumption('inferred-pin-role:D2/1', 'inferred_pin_role', 'D2'),
  assumption('inferred-pin-role:D3/1', 'inferred_pin_role', 'D3'),
]

const run = {
  ok: true, passed: false, assumptions: registry, export_id: 'x',
  results: [{
    label: 'no faults', kind: 'no_faults', passed: false, invalid: true, detail: '',
    evidence: { assertion: 'no_faults', status: 'undermined', assumptions: registry.map(a => a.id) },
  }],
} as unknown as RunResponse

const actions: RemedyActions = { addSupply: noop, markNotFitted: noop, openModel: noop, applied: () => false }
const TOML = '[[assert]]\nkind = "no_faults"\n'
const rows = tomlToBuilder(TOML)!.checks

describe('a run that is not judged states it once', () => {
  const band = renderToStaticMarkup(
    <VerdictBand
      result={run} runKey="k" stale={false} running={false} canRun onRun={noop} validationCount={0}
      reportSerious={0} onJump={noop} specFileName="s.toml" specToml={TOML} rawMode={false}
      preLayoutNotice={null} remedyActions={actions} exportMenu={null}
    />,
  )
  const list = renderToStaticMarkup(
    <AssertionGroups
      checks={rows} validation={new Map()} stale={false} resultForRow={id => run.ok ? run.results![id - 1] ?? null : null}
      assumptions={registry} inventory={[]} onAdd={noop} onUpdate={noop} onRemove={noop} bandOffersModelling
    />,
  )

  test('the band owns the status word and the one modelling action', () => {
    expect(band).toContain('Not judged yet: 1 check waits on models for 2 parts.')
    expect(band).toContain('Model the 2 open parts')
  })

  test('the row keeps its specific reason and part chips, without a second pill or button', () => {
    expect(list).toContain('data-testid="not-judged"')
    expect(list).toContain('2 parts have none yet')
    expect(list).toContain('data-testid="not-judged-parts"')
    expect(list).not.toContain('data-testid="row-result"')
    expect(list).not.toContain('INVALID')
    expect(list).not.toContain('data-testid="not-judged-open"')
    expect(list).not.toContain('Model them')
  })

  test('when the band does not offer modelling (a failing run), the row keeps its button', () => {
    const alone = renderToStaticMarkup(
      <AssertionEvidence result={run.ok ? run.results![0] : (null as never)} assumptions={registry} inventory={[]} repeats={new Set()} stale={false} />,
    )
    expect(alone).toContain('data-testid="not-judged-open"')
  })

  test('the formal word, wherever it is still shown, comes with its plain meaning', () => {
    const chip = renderToStaticMarkup(<ResultChip result={run.ok ? run.results![0] : (null as never)} stale={false} />)
    expect(chip).toContain('INVALID · NOT JUDGED')
    const raw = renderToStaticMarkup(<RunResults result={run} overall="invalid" stale={false} rawMode />)
    expect(raw).toContain('INVALID · NOT JUDGED')
    expect(raw).not.toContain('data-testid="not-judged-open"')
  })
})

describe('"it also rests on" is a disclosure of linked parts', () => {
  const html = renderToStaticMarkup(
    <AssertionEvidence
      part="glance" offerModel={false} result={run.ok ? run.results![0] : (null as never)}
      assumptions={registry} inventory={[]} repeats={new Set()} stale={false}
    />,
  )

  test('says how many parts, behind a closed details', () => {
    expect(html).toMatch(/<details[^>]*data-testid="other-assumptions"(?![^>]*open)/)
    expect(html).toContain('It also rests on 2 assumed parts')
    expect(html).not.toContain('Why lists them')
  })

  test('lists each part as a chip that goes to the board', () => {
    const inner = html.slice(html.indexOf('data-testid="other-assumptions"'))
    expect(inner).toContain('data-testid="other-assumption-chips"')
    for (const part of ['D2', 'D3']) expect(inner).toContain(`data-name="${part}"`)
    expect(inner).toContain('Inferred pin role')
  })
})

describe('timing notes', () => {
  test('name the emulator backend in words, not as a raw id', () => {
    const result = {
      ok: true, passed: true, results: [],
      timing_coverage: [{ mcu_ref: 'U1', backend: 'simavr:atmega328p', timestamp_precision_s: 1e-6, minimum_guaranteed_pulse_s: 1e-6, cycle_exact: true }],
    } as unknown as RunResponse
    const html = renderToStaticMarkup(<RunResults result={result} overall="passed" stale={false} rawMode={false} />)
    expect(html).toContain(`emulated as ${labelFor('simavr:atmega328p')}`)
    expect(labelFor('simavr:atmega328p')).toContain('simavr')
    expect(html).not.toContain('simavr:atmega328p')
  })
})

// ── 5. Agent activity in plain words ────────────────────────────────────────

const fold = (events: object[]) =>
  events.reduce<ActivityState>((s, e) => applyActivityLine(s, `activity: ${JSON.stringify(e)}`) ?? s, initialActivity())

describe('agent activity', () => {
  const state = fold([
    { t: 1, kind: 'tier', model: 'claude-sonnet-5-5', tier: 2, tiers: 3, harness: 'staged', budget: 100000, label: 'Sonnet 5.5' },
    { t: 2, kind: 'stage', stage: 'pins', attempt: 1, attempts: 2 },
    { t: 3, kind: 'validator', ok: true },
    { t: 4, kind: 'escalate', from: 'claude-haiku-5-5', to: 'claude-sonnet-5-5', reason: 'validator failed' },
  ])
  const html = renderToStaticMarkup(<AgentActivity activity={state} />)

  test('says which attempt this is and that a later one is a stronger model', () => {
    expect(html).toContain('Attempt 2 of 3 (stronger model)')
    expect(attemptWords(1, 3)).toBe('Attempt 1 of 3')
  })

  test('says what the check found in words', () => {
    expect(html).toContain('values are consistent')
    const failed = renderToStaticMarkup(<AgentActivity activity={fold([{ t: 1, kind: 'validator', ok: false, error: 'pin 3 missing' }])} />)
    expect(failed).toContain('problem found: pin 3 missing')
  })

  test('shows no tier or validator jargon anywhere, feed included', () => {
    // The test ids and the server's own reason string are not wording the page invents.
    const words = html.replace(/data-testid="[^"]*"/g, '').replaceAll('validator failed', '')
    expect(words).not.toMatch(/\btier\b/i)
    expect(words).not.toMatch(/validator/i)
    expect(html).not.toContain('harnessed')
    expect(html).toContain('Moved to a stronger model')
    expect(html).toContain('try 1 of 2')
  })

  test('the finished line reads as plain words too', () => {
    const done = renderToStaticMarkup(<AgentActivity activity={fold([
      { t: 1, kind: 'tier', model: 'm', tier: 1, tiers: 2, harness: 'single', budget: 0 },
      { t: 2, kind: 'final', model: 'm', tier: 1, tiers: 2, escalations: [], confidence: 'high' },
    ])} />)
    expect(done).toContain('finished on attempt 1 of 2, confidence high')
    expect(done.replace(/data-testid="[^"]*"/g, '')).not.toMatch(/\btier\b/i)
  })
})
