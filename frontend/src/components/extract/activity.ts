import type { ActivityEvent } from '../../types/report'

// The pure half of the live "watch the agent" panel: the server's `activity:
// <json>` lines in, one small view state out. Kept free of React so the whole
// thing is unit-testable, and so a malformed line is a plain `null` here rather
// than an exception inside a render.

/** The prefix a progress line carries when its payload is an activity event. */
export const ACTIVITY_PREFIX = 'activity: '

/** How many events the scrolling feed keeps. */
export const FEED_LIMIT = 30

export interface FeedItem {
  t: number
  text: string
}

/** As the server reports it: dollars are shown only for "metered". */
export type Billing = 'metered' | 'plan' | 'unknown'

/** A plan-usage window as percent of the limit, before the run and after it.
 *  Only built when the server sent both ends. */
export interface UsageSpan {
  from: number
  to: number
}

/** The end-of-run `plan` event: usage before and after a subscription run. */
export interface PlanUsage {
  /** False when the server could not read plan usage. */
  available: boolean
  week?: UsageSpan
  /** The 5-hour window. */
  session?: UsageSpan
}

export interface ActivityState {
  /** The tier now running, from the latest `tier` event. */
  tier: { model: string; tier: number; tiers: number; harness: string; budget: number; label?: string; badge?: string } | null
  stage: { stage: string; attempt: number; attempts: number } | null
  /** Datasheet pages the agent has read, ascending and unique. */
  pages: number[]
  escalations: { from: string; to: string; reason: string }[]
  /** Tokens spent so far across the whole run (0 until a `usage` event). */
  tokens: number
  /** Run cost so far at list price; null when the server could not price it.
   *  Shown only when `billing` is "metered". */
  costUsd: number | null
  /** How the server says the run is billed; "unknown" until it says. */
  billing: Billing
  /** The end-of-run plan usage, once the server has sent it. */
  plan: PlanUsage | null
  validator: { ok: boolean; error?: string } | null
  final: { model: string; tier: number; tiers: number; escalations: string[]; confidence: string } | null
  /** The last `FEED_LIMIT` events, oldest first. */
  feed: FeedItem[]
  /** Events seen, so the panel can stay hidden until the first one. */
  count: number
}

export const initialActivity = (): ActivityState => ({
  tier: null,
  stage: null,
  pages: [],
  escalations: [],
  tokens: 0,
  costUsd: null,
  billing: 'unknown',
  plan: null,
  validator: null,
  final: null,
  feed: [],
  count: 0,
})

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v !== null && !Array.isArray(v)

const KINDS: ReadonlySet<string> = new Set([
  'tier', 'stage', 'read', 'tool', 'usage', 'plan', 'validator', 'escalate', 'final', 'note',
])

/** Reads one progress line. Returns null for anything that is not a well-formed
 *  activity event (not the prefix, not JSON, not an object with a known `kind`),
 *  so the caller can drop it without caring why. */
export function parseActivityLine(line: string): ActivityEvent | null {
  if (!line.startsWith(ACTIVITY_PREFIX)) return null
  let raw: unknown
  try {
    raw = JSON.parse(line.slice(ACTIVITY_PREFIX.length))
  } catch {
    return null
  }
  if (!isRecord(raw) || typeof raw.kind !== 'string' || !KINDS.has(raw.kind)) return null
  return { ...raw, t: typeof raw.t === 'number' && Number.isFinite(raw.t) ? raw.t : 0 } as ActivityEvent
}

const str = (v: unknown, fallback = ''): string => (typeof v === 'string' ? v : fallback)
const num = (v: unknown, fallback = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : fallback)

/** "$0.0123 list price"; "cost unknown" when the server sent none. Callers
 *  use it only for metered billing; see `spendLines`. */
export function formatCost(usd: number | null): string {
  return usd === null ? 'cost unknown' : `$${usd.toFixed(4)} list price`
}

const pct = (n: number): string => `${Number(n.toFixed(1))}%`
const delta = (from: number, to: number): string => {
  const d = Number((to - from).toFixed(1))
  return `${d < 0 ? '-' : '+'}${Math.abs(d)}`
}

/** "Plan usage: week 46% \u2192 47% (+1), 5-hour 4% \u2192 6% (+2)". A window the
 *  server sent no number for is left out, and nothing is ever estimated. When
 *  the server could not read usage (or no window is left) it says so. */
export function planLine(plan: PlanUsage): string {
  const parts = plan.available
    ? ([['week', plan.week], ['5-hour', plan.session]] as const).flatMap(([name, w]) =>
        w ? [`${name} ${pct(w.from)} \u2192 ${pct(w.to)} (${delta(w.from, w.to)})`] : [])
    : []
  return parts.length ? `Plan usage: ${parts.join(', ')}` : 'usage unavailable'
}

/** What to show for the run's spend, from the server's billing report alone.
 *  A dollar figure appears only for "metered" billing. A "plan" (subscription)
 *  run says "from your plan", plus the plan-usage line once it arrives. An
 *  unknown or missing mode says "billing unknown". There is no fallback to
 *  dollars, so a cost value on any other mode is never shown. */
export function spendLines(state: ActivityState): string[] {
  const lines: string[] = []
  if (state.billing === 'metered') lines.push(formatCost(state.costUsd))
  else if (state.billing === 'plan') lines.push('from your plan')
  else lines.push('billing unknown')
  if (state.plan) lines.push(planLine(state.plan))
  return lines
}

/** 1234 -> "1.2k", 1_500_000 -> "1.5M"; small counts stay whole. */
export function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`
  return String(Math.round(n))
}

/** "harnessed: ~100k-token stages" for a model with a context budget, else null. */
export function budgetLabel(budget: number | undefined): string | null {
  if (!budget || budget <= 0) return null
  return `harnessed: ~${formatTokens(budget).replace(/\.0(?=[kM]$)/, '')}-token stages`
}

/** One window's before/after, only when the server sent a number for both. */
const win = (before: unknown, after: unknown, key: 'week' | 'session'): UsageSpan | undefined => {
  const from = isRecord(before) ? before[key] : undefined
  const to = isRecord(after) ? after[key] : undefined
  return typeof from === 'number' && Number.isFinite(from) && typeof to === 'number' && Number.isFinite(to)
    ? { from, to }
    : undefined
}

/** A `plan` event's fields as plan usage. Only `available: true` counts as
 *  readable, and a window missing either end is left out. */
function planFrom(v: Record<string, unknown>): PlanUsage {
  const week = win(v.before, v.after, 'week')
  const session = win(v.before, v.after, 'session')
  return { available: v.available === true, ...(week ? { week } : {}), ...(session ? { session } : {}) }
}

/** One feed line per event. Every field is read defensively: the server is
 *  newer or older than this page often enough that a missing one must not throw. */
export function describeEvent(e: ActivityEvent): string {
  const v = e as unknown as Record<string, unknown>
  switch (e.kind) {
    case 'tier':
      return `tier ${num(v.tier)} of ${num(v.tiers)}: ${str(v.model)} (${str(v.harness, 'single')})`
    case 'stage':
      return `stage ${str(v.stage)}, attempt ${num(v.attempt, 1)} of ${num(v.attempts, 1)}`
    case 'read':
      return `read ${str(v.path)}${typeof v.page === 'number' ? ` (page ${v.page})` : ''}`
    case 'tool':
      return `${str(v.name, 'tool')}${v.detail ? `: ${str(v.detail)}` : ''}`
    case 'usage':
      // A turn's context reading and a finished call's totals arrive as separate events.
      return [
        typeof v.total_tokens === 'number' ? `${formatTokens(v.total_tokens)} tokens so far` : '',
        typeof v.context === 'number' ? `context ${formatTokens(v.context)}` : '',
      ].filter(Boolean).join(', ') || 'usage'
    case 'plan':
      return planLine(planFrom(v))
    case 'validator':
      return v.ok ? 'validator: ok' : `validator: ${str(v.error, 'failed')}`
    case 'escalate':
      return `escalate ${str(v.from)} -> ${str(v.to)}: ${str(v.reason)}`
    case 'final':
      return `done on tier ${num(v.tier)} of ${num(v.tiers)} (${str(v.model)}), confidence ${str(v.confidence, 'unknown')}`
    case 'note':
      return str(v.text)
  }
}

/** Folds one event into the view state. Pure: returns a new state. */
export function reduceActivity(state: ActivityState, event: ActivityEvent): ActivityState {
  const v = event as unknown as Record<string, unknown>
  const next: ActivityState = {
    ...state,
    count: state.count + 1,
    feed: [...state.feed, { t: event.t, text: describeEvent(event) }].slice(-FEED_LIMIT),
  }
  switch (event.kind) {
    case 'tier':
      // A new tier starts its own stages and validation; reads and spend carry on.
      next.tier = {
        model: str(v.model), tier: num(v.tier, 1), tiers: num(v.tiers, 1),
        harness: str(v.harness, 'single'), budget: num(v.budget),
        ...(typeof v.label === 'string' && v.label ? { label: v.label } : {}),
        ...(typeof v.badge === 'string' && v.badge ? { badge: v.badge } : {}),
      }
      next.stage = null
      next.validator = null
      break
    case 'stage':
      next.stage = { stage: str(v.stage), attempt: num(v.attempt, 1), attempts: num(v.attempts, 1) }
      break
    case 'read':
      if (typeof v.page === 'number' && Number.isInteger(v.page) && !state.pages.includes(v.page)) {
        next.pages = [...state.pages, v.page].sort((a, b) => a - b)
      }
      break
    case 'usage':
      if (typeof v.total_tokens === 'number') next.tokens = v.total_tokens
      // Whatever the server calls the billing mode, only these two are known;
      // the cost is kept but only ever shown for "metered".
      if (typeof v.billing === 'string') next.billing = v.billing === 'metered' || v.billing === 'plan' ? v.billing : 'unknown'
      if (typeof v.total_cost_usd === 'number') next.costUsd = v.total_cost_usd
      break
    case 'plan':
      next.plan = planFrom(v)
      break
    case 'validator':
      next.validator = { ok: v.ok === true, ...(typeof v.error === 'string' ? { error: v.error } : {}) }
      break
    case 'escalate':
      next.escalations = [...state.escalations, { from: str(v.from), to: str(v.to), reason: str(v.reason) }]
      break
    case 'final':
      next.final = {
        model: str(v.model), tier: num(v.tier, 1), tiers: num(v.tiers, 1),
        escalations: Array.isArray(v.escalations) ? v.escalations.map(r => str(r)) : [],
        confidence: str(v.confidence, 'unknown'),
      }
      break
    case 'tool':
    case 'note':
      break
  }
  return next
}

/** Applies a raw progress line: the new state when it was an activity event,
 *  `null` when it was not an activity line at all, and the unchanged state when
 *  it was one but malformed (so the caller still keeps it out of the plain log). */
export function applyActivityLine(state: ActivityState, line: string): ActivityState | null {
  if (!line.startsWith(ACTIVITY_PREFIX)) return null
  const event = parseActivityLine(line)
  return event ? reduceActivity(state, event) : state
}
