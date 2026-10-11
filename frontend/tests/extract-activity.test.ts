import { expect, test } from 'bun:test'
import {
  applyActivityLine, budgetLabel, FEED_LIMIT, formatCost, initialActivity, parseActivityLine, planLine, reduceActivity, spendLines,
} from '../src/components/extract/activity'
import { escalateFromText, escalateToText, labelMap, ladderLabel } from '../src/components/extract/escalate'

const line = (e: object) => `activity: ${JSON.stringify(e)}`
const fold = (events: object[]) =>
  events.reduce<ReturnType<typeof initialActivity>>((s, e) => applyActivityLine(s, line(e)) ?? s, initialActivity())

test('a run folds into tier, stage, pages, spend, validator and escalations', () => {
  const s = fold([
    { t: 0.1, kind: 'tier', model: 'claude-haiku-5-5', tier: 1, tiers: 3, harness: 'staged', budget: 100000 },
    { t: 1, kind: 'stage', stage: 'locate', attempt: 1, attempts: 2 },
    { t: 2, kind: 'read', path: 'pages/p7.png', page: 7 },
    { t: 3, kind: 'read', path: 'pages/p3.png', page: 3 },
    { t: 3.5, kind: 'read', path: 'pages/p7.png', page: 7 },
    { t: 3.6, kind: 'read', path: 'notes.txt' },
    { t: 4, kind: 'usage', model: 'claude-haiku-5-5', context: 5000, input: 1, output: 1, cache_read: 0, cache_write: 0, cost_usd: 0.01, total_tokens: 12345, total_cost_usd: 0.0123 },
    { t: 5, kind: 'validator', ok: false, error: 'pin 3 missing' },
    { t: 6, kind: 'escalate', from: 'claude-haiku-5-5', to: 'claude-sonnet-5-5', reason: 'validator failed' },
    { t: 7, kind: 'tier', model: 'claude-sonnet-5-5', tier: 2, tiers: 3, harness: 'staged', budget: 0 },
  ])
  expect(s.tier).toEqual({ model: 'claude-sonnet-5-5', tier: 2, tiers: 3, harness: 'staged', budget: 0 })
  expect(s.stage).toBeNull() // a new tier starts its own stages
  expect(s.validator).toBeNull()
  expect(s.pages).toEqual([3, 7])
  expect(s.tokens).toBe(12345)
  expect(s.costUsd).toBe(0.0123)
  expect(s.escalations).toEqual([{ from: 'claude-haiku-5-5', to: 'claude-sonnet-5-5', reason: 'validator failed' }])
  expect(s.count).toBe(10)
})

test('the validator keeps the latest result and an unpriced usage keeps the old cost', () => {
  const s = fold([
    { t: 1, kind: 'usage', model: 'm', context: 1, input: 1, output: 1, cache_read: 0, cache_write: 0, cost_usd: null, total_tokens: 10, total_cost_usd: 0.5 },
    { t: 2, kind: 'usage', model: 'm', context: 1, input: 1, output: 1, cache_read: 0, cache_write: 0, cost_usd: null, total_tokens: 20, total_cost_usd: null },
    { t: 3, kind: 'validator', ok: false, error: 'bad' },
    { t: 4, kind: 'validator', ok: true },
    { t: 5, kind: 'final', model: 'm', tier: 1, tiers: 2, escalations: [], confidence: 'high' },
  ])
  expect(s.tokens).toBe(20)
  expect(s.costUsd).toBe(0.5)
  expect(s.validator).toEqual({ ok: true })
  expect(s.final?.confidence).toBe('high')
})

test('a malformed activity line is ignored and does not throw', () => {
  const s = reduceActivity(initialActivity(), { t: 1, kind: 'note', text: 'hi' })
  for (const bad of ['activity: {not json', 'activity: ', 'activity: null', 'activity: [1]', 'activity: 3',
    'activity: {"t":1}', 'activity: {"t":1,"kind":"mystery"}']) {
    expect(parseActivityLine(bad)).toBeNull()
    expect(applyActivityLine(s, bad)).toBe(s) // consumed (kept out of the log) but unchanged
  }
  expect(applyActivityLine(s, 'phase: reading')).toBeNull() // not an activity line at all
  // Missing or wrong-typed fields on a known kind degrade instead of throwing.
  const odd = fold([{ kind: 'tier' }, { t: 'x', kind: 'usage', total_tokens: 'many' }, { kind: 'escalate' }, { kind: 'read', page: 'two' }])
  expect(odd.count).toBe(4)
  expect(odd.tokens).toBe(0)
  expect(odd.pages).toEqual([])
})

test('the feed keeps only the most recent events', () => {
  const s = fold(Array.from({ length: FEED_LIMIT + 10 }, (_, i) => ({ t: i, kind: 'note', text: `n${i}` })))
  expect(s.feed).toHaveLength(FEED_LIMIT)
  expect(s.feed[0].text).toBe('n10')
  expect(s.feed.at(-1)?.text).toBe(`n${FEED_LIMIT + 9}`)
  expect(s.count).toBe(FEED_LIMIT + 10)
})

test('cost and budget wording', () => {
  expect(formatCost(0.0123)).toBe('$0.0123 list price')
  expect(formatCost(null)).toBe('cost unknown')
  expect(budgetLabel(100000)).toBe('harnessed: ~100k-token stages')
  expect(budgetLabel(150000)).toBe('harnessed: ~150k-token stages')
  expect(budgetLabel(0)).toBeNull()
  expect(budgetLabel(undefined)).toBeNull()
})

test('the escalate field: blank is default, none is an empty list, otherwise ids', () => {
  expect(escalateFromText('')).toBeUndefined()
  expect(escalateFromText('  ')).toBeUndefined()
  expect(escalateFromText('none')).toEqual([])
  expect(escalateFromText(' None ')).toEqual([])
  expect(escalateFromText('claude-sonnet-5-5, claude-opus-5-5')).toEqual(['claude-sonnet-5-5', 'claude-opus-5-5'])
  expect(escalateToText(undefined)).toBe('')
  expect(escalateToText([])).toBe('none')
  expect(escalateToText(['a', 'b'])).toBe('a, b')
  expect(escalateFromText(escalateToText(['a', 'b']))).toEqual(['a', 'b'])
})

test('a ladder uses the server\'s labels and shows any id it has none for as written', () => {
  const labels = labelMap([
    { name: 'claude-haiku-5-5', label: 'Haiku 5.5' },
    { name: 'claude-sonnet-5-5', label: 'Sonnet 5.5' },
    { name: 'claude-opus-5-5' },
  ])
  expect(labels).toEqual({ 'claude-haiku-5-5': 'Haiku 5.5', 'claude-sonnet-5-5': 'Sonnet 5.5' })
  expect(ladderLabel('claude-haiku-5-5', ['claude-sonnet-5-5', 'claude-opus-5-5'], labels)).toBe('Haiku 5.5 → Sonnet 5.5 → claude-opus-5-5')
  expect(ladderLabel('claude-haiku-5-5', ['claude-sonnet-5-5'])).toBe('claude-haiku-5-5 → claude-sonnet-5-5')
  expect(ladderLabel('gpt-6-luna', ['gpt-6.1-sol'])).toBe('gpt-6-luna → gpt-6.1-sol')
})

test('a tier carries the server\'s label and baseline badge when sent', () => {
  const s = fold([{ t: 1, kind: 'tier', model: 'claude-haiku-5-5', tier: 1, tiers: 2, harness: 'staged', budget: 1, label: 'Haiku 5.5', badge: 'below Luna baseline' }])
  expect(s.tier?.label).toBe('Haiku 5.5')
  expect(s.tier?.badge).toBe('below Luna baseline')
  expect(fold([{ t: 1, kind: 'tier', model: 'm', tier: 1, tiers: 1, harness: 'single', budget: 0 }]).tier?.badge).toBeUndefined()
})

const usage = (extra: object) => ({ t: 1, kind: 'usage', model: 'm', context: 1, input: 1, output: 1, cache_read: 0, cache_write: 0,
  cost_usd: 0.5, total_tokens: 10, total_cost_usd: 0.5, ...extra })
const plan = (extra: object) => ({ t: 9, kind: 'plan', tool: 'claude', available: true,
  before: { week: 46, session: 4 }, after: { week: 47, session: 6 }, ...extra })

test('a dollar figure appears only when the server reports metered billing', () => {
  expect(spendLines(fold([usage({ billing: 'metered', total_cost_usd: 0.0123 })]))).toEqual(['$0.0123 list price'])
  expect(spendLines(fold([usage({ billing: 'metered', total_cost_usd: null })]))).toEqual(['cost unknown'])
  // A cost value on any other mode, or with no billing report, is never shown as money.
  expect(spendLines(fold([usage({ billing: 'plan' })]))).toEqual(['from your plan'])
  expect(spendLines(fold([usage({ billing: 'unknown' })]))).toEqual(['billing unknown'])
  expect(spendLines(fold([usage({ billing: 'something-new' })]))).toEqual(['billing unknown'])
  expect(spendLines(fold([usage({})]))).toEqual(['billing unknown'])
  expect(spendLines(initialActivity())).toEqual(['billing unknown'])
  for (const billing of ['plan', 'unknown', undefined]) {
    expect(spendLines(fold([usage({ billing, plan: undefined })])).join(' ')).not.toContain('$')
  }
})

test('the plan line shows week and 5-hour usage with deltas, and never invents a number', () => {
  expect(planLine({ available: true, week: { from: 46, to: 47 }, session: { from: 4, to: 6 } }))
    .toBe('Plan usage: week 46% → 47% (+1), 5-hour 4% → 6% (+2)')
  const folded = fold([usage({ billing: 'plan' }), plan({})])
  expect(spendLines(folded)).toEqual(['from your plan', 'Plan usage: week 46% → 47% (+1), 5-hour 4% → 6% (+2)'])
  // A window whose value is null (either end) is left out.
  expect(spendLines(fold([usage({ billing: 'plan' }), plan({ before: { week: 46, session: null }, after: { week: 47, session: 6 } })])))
    .toEqual(['from your plan', 'Plan usage: week 46% → 47% (+1)'])
  expect(spendLines(fold([usage({ billing: 'plan' }), plan({ before: { week: null, session: 4 }, after: { week: null, session: 6 } })])))
    .toEqual(['from your plan', 'Plan usage: 5-hour 4% → 6% (+2)'])
  // Fractions, falls and no change.
  expect(planLine({ available: true, week: { from: 10.25, to: 12 }, session: { from: 5, to: 3 } }))
    .toBe('Plan usage: week 10.3% → 12% (+1.8), 5-hour 5% → 3% (-2)')
  expect(planLine({ available: true, week: { from: 5, to: 5 } })).toBe('Plan usage: week 5% → 5% (+0)')
  // Unavailable, or nothing usable in it: say so, show no percentage.
  expect(spendLines(fold([usage({ billing: 'plan' }), plan({ available: false })])))
    .toEqual(['from your plan', 'usage unavailable'])
  expect(spendLines(fold([usage({ billing: 'plan' }), plan({ available: false, before: { week: 1, session: 1 }, after: { week: 2, session: 2 } })])))
    .toEqual(['from your plan', 'usage unavailable'])
  expect(spendLines(fold([usage({ billing: 'plan' }), plan({ before: undefined, after: undefined })])))
    .toEqual(['from your plan', 'usage unavailable'])
  expect(spendLines(fold([usage({ billing: 'plan' }), { t: 9, kind: 'plan' }])))
    .toEqual(['from your plan', 'usage unavailable'])
  // The plan event also reads in the feed.
  expect(folded.feed.at(-1)?.text).toBe('Plan usage: week 46% → 47% (+1), 5-hour 4% → 6% (+2)')
})
