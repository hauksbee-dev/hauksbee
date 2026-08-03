import { useCallback, useEffect, useState } from 'react'

// The Hauksbee Cloud waitlist: one email field posting to the demo Worker's
// /api/waitlist (KV-backed, no third party). The endpoint answers GET with
// 204 as its liveness probe; without it (vite dev, a bare static server) the
// form disables itself and says why instead of collecting emails into a void.

type Availability = 'checking' | 'up' | 'down'
type SubmitState =
  | { phase: 'idle' }
  | { phase: 'sending' }
  | { phase: 'done'; message: string }
  | { phase: 'error'; message: string }

export function WaitlistCard() {
  const [avail, setAvail] = useState<Availability>('checking')
  const [email, setEmail] = useState('')
  const [submit, setSubmit] = useState<SubmitState>({ phase: 'idle' })

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        // Must be the worker's own 204: an SPA fallback answers any GET with
        // a 200 HTML page, which is exactly the false positive to reject.
        const res = await fetch('/api/waitlist', { method: 'GET' })
        if (alive) setAvail(res.status === 204 ? 'up' : 'down')
      } catch {
        if (alive) setAvail('down')
      }
    })()
    return () => { alive = false }
  }, [])

  const onSubmit = useCallback(async (e: React.FormEvent) => {
    e.preventDefault()
    if (avail !== 'up' || submit.phase === 'sending') return
    setSubmit({ phase: 'sending' })
    try {
      const res = await fetch('/api/waitlist', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email: email.trim() }),
      })
      const body = await res.json().catch(() => null) as { ok?: boolean; message?: string; error?: string } | null
      if (!res.ok || !body?.ok) {
        throw new Error(body?.error ?? `the waitlist endpoint answered ${res.status}`)
      }
      setSubmit({ phase: 'done', message: body.message ?? 'You are on the list.' })
    } catch (err) {
      setSubmit({ phase: 'error', message: err instanceof Error ? err.message : String(err) })
    }
  }, [avail, email, submit.phase])

  return (
    <section
      data-testid="waitlist-card"
      className="hb-card px-4 py-3.5"
      style={{ borderRadius: 10, textAlign: 'left' }}
    >
      <div className="text-[10px] font-bold tracking-[0.14em] uppercase mb-1" style={{ color: 'var(--copper)' }}>
        Hauksbee Cloud
      </div>
      <div className="text-[13px] font-semibold mb-1" style={{ color: 'var(--silk)' }}>
        Hosted CI: your boards checked on every push
      </div>
      <p className="text-[12px] leading-relaxed mb-2.5" style={{ color: 'var(--silk-dim)', margin: '0 0 10px' }}>
        Join the waitlist and we will email you once, when it opens. Nothing
        else is done with the address.
      </p>

      {submit.phase === 'done' ? (
        <div
          data-testid="waitlist-done"
          className="text-[12px] px-3 py-2 rounded-lg"
          style={{ background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' }}
        >
          {submit.message}
        </div>
      ) : (
        <form onSubmit={e => void onSubmit(e)} className="flex gap-1.5">
          <input
            type="email"
            required
            value={email}
            onChange={e => setEmail(e.target.value)}
            placeholder="you@example.com"
            disabled={avail !== 'up'}
            data-testid="waitlist-email"
            className="flex-1 min-w-0 px-2.5 text-[12px] rounded-lg outline-none"
            style={{
              height: 32,
              background: 'var(--surface-2)',
              border: '1px solid var(--hairline)',
              color: 'var(--silk)',
              opacity: avail === 'up' ? 1 : 0.55,
            }}
          />
          <button
            type="submit"
            disabled={avail !== 'up' || submit.phase === 'sending'}
            data-testid="waitlist-join"
            className="hb-btn-primary hb-press px-3 text-[12px] whitespace-nowrap"
            style={{ height: 32, opacity: avail === 'up' ? 1 : 0.55 }}
          >
            {submit.phase === 'sending' ? 'Joining ...' : 'Join the waitlist'}
          </button>
        </form>
      )}

      {avail === 'down' && (
        <p data-testid="waitlist-offline" className="text-[11px] mt-2" style={{ color: 'var(--silk-faint)', margin: '8px 0 0' }}>
          The waitlist endpoint is not running in this local build; it comes
          up with the hosted demo's Worker.
        </p>
      )}
      {submit.phase === 'error' && (
        <p className="text-[11px] mt-2" style={{ color: 'var(--err)', margin: '8px 0 0' }}>
          {submit.message}
        </p>
      )}
    </section>
  )
}
