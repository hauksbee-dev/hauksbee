import { useEffect, useRef } from 'react'
import { type ActivityState, attemptWords, formatTokens, spendLines, VALIDATOR_OK } from './activity'

// A compact live view of what the extraction agent is doing, driven by the
// `activity:` events the server streams alongside the plain progress lines.
// Deliberately plain: one row of facts and a short feed. Nothing renders until
// the first event, so a server that sends none (or an older one) shows nothing.

const clock = (t: number) => {
  const s = Math.floor(t)
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`
}

function Fact({ label, testId, children }: { label: string; testId: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0" data-testid={testId}>
      <div className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>{label}</div>
      <div className="text-[12px] leading-snug break-words" style={{ color: 'var(--silk)' }}>{children}</div>
    </div>
  )
}

export function AgentActivity({ activity }: { activity: ActivityState }) {
  const feedRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const el = feedRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [activity.feed])

  if (activity.count === 0) return null
  const { tier, stage, pages, escalations, validator } = activity
  // A model with a small context works through the datasheet in fixed-size steps.
  const stepSize = tier && tier.budget > 0 ? `reads in ~${formatTokens(tier.budget).replace(/\.0(?=[kM]$)/, '')}-token steps` : null
  const mono = { fontFamily: 'var(--font-mono)' }

  return (
    <div
      data-testid="agent-activity"
      className="mt-2.5 rounded-lg px-3 py-2.5"
      style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
    >
      <div className="text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
        Agent activity
      </div>

      <div className="mt-1.5 grid gap-x-4 gap-y-2" style={{ gridTemplateColumns: 'repeat(auto-fit, minmax(10rem, 1fr))' }}>
        <Fact label="Model" testId="agent-activity-tier">
          {tier ? (
            <>
              <span style={mono} title={tier.model}>{tier.label ?? tier.model}</span>
              <span style={{ color: 'var(--silk-dim)' }}> · {attemptWords(tier.tier, tier.tiers)}{stepSize && ` · ${stepSize}`}</span>
              {tier.badge && <span data-testid="agent-activity-badge" style={{ color: 'var(--copper-hi)' }}> · {tier.badge}</span>}
            </>
          ) : '...'}
        </Fact>
        <Fact label="Step" testId="agent-activity-stage">
          {stage ? `${stage.stage} (try ${stage.attempt} of ${stage.attempts})` : '...'}
        </Fact>
        <Fact label="Pages read" testId="agent-activity-pages">
          <span style={mono}>{pages.length ? pages.join(', ') : 'none yet'}</span>
        </Fact>
        <Fact label="Spend so far" testId="agent-activity-spend">
          {formatTokens(activity.tokens)} tokens
          {spendLines(activity).map(line => (
            <div key={line} data-testid="agent-activity-spend-line" style={{ color: 'var(--silk-dim)' }}>{line}</div>
          ))}
        </Fact>
        <Fact label="Checked" testId="agent-activity-validator">
          {validator === null ? 'not yet' : validator.ok ? VALIDATOR_OK : (
            <span style={{ color: 'var(--warn, var(--silk))' }}>problem found: {validator.error ?? 'no detail'}</span>
          )}
        </Fact>
        {activity.final && (
          <Fact label="Result" testId="agent-activity-final">
            finished on {attemptWords(activity.final.tier, activity.final.tiers).toLowerCase()}, confidence {activity.final.confidence}
          </Fact>
        )}
      </div>

      {escalations.length > 0 && (
        <ul className="mt-2 text-[12px] leading-snug" data-testid="agent-activity-escalations" style={{ color: 'var(--silk-dim)' }}>
          {escalations.map((e, i) => (
            <li key={i}>
              <span className="font-semibold" style={{ color: 'var(--silk)' }}>Moved to a stronger model</span>{' '}
              <span style={mono}>{e.from}</span> &rarr; <span style={mono}>{e.to}</span>: {e.reason}
            </li>
          ))}
        </ul>
      )}

      <div
        ref={feedRef}
        data-testid="agent-activity-feed"
        className="mt-2 rounded-md px-2.5 py-1.5 text-[11px] overflow-y-auto"
        style={{ maxHeight: 120, background: 'var(--instrument)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)', ...mono }}
      >
        {activity.feed.map((item, i) => (
          <div key={i} className="whitespace-pre-wrap break-words">
            <span style={{ color: 'var(--silk-faint)' }}>{clock(item.t)}</span> {item.text}
          </div>
        ))}
      </div>
    </div>
  )
}
