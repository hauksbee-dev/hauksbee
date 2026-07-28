// The hauksbee.dev demo Worker: static assets for the demo build, plus the
// one dynamic endpoint the demo has, the Hauksbee Cloud waitlist. Emails go
// straight to KV; no third-party service ever sees them, and the stored
// record is the address plus the join timestamp, nothing else.
//
// Contract (the frontend's WaitlistCard is the only client):
//   GET  /api/waitlist            204, the liveness probe the form gates on.
//                                 An SPA fallback would answer 200 HTML, so
//                                 the client requires exactly 204.
//   POST /api/waitlist {email}    { ok: true, message } on success, including
//                                 re-joins (idempotent; no "already exists"
//                                 leak of who is on the list).
//                                 { ok: false, error } with 4xx on bad input.
//   anything else                 the static demo build (SPA fallback).

// Enough for "is this worth storing", without rejecting real addresses the
// way stricter patterns do. The 254-octet cap is RFC 5321's.
const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/

const json = (status, body) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })

async function handleJoin(request, env) {
  let email
  try {
    const body = await request.json()
    email = typeof body.email === 'string' ? body.email.trim().toLowerCase() : ''
  } catch {
    return json(400, { ok: false, error: 'the request body must be JSON with an "email" field' })
  }
  if (!email || email.length > 254 || !EMAIL_RE.test(email)) {
    return json(400, { ok: false, error: 'that does not look like an email address' })
  }
  // Idempotent put: re-joining refreshes the record rather than erroring,
  // so the confirmation can be honest without leaking membership.
  await env.WAITLIST.put(
    `waitlist:${email}`,
    JSON.stringify({ email, joined_at: new Date().toISOString() }),
  )
  return json(200, {
    ok: true,
    message: 'You are on the list. One email when Hauksbee Cloud opens, nothing else.',
  })
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url)
    if (url.pathname === '/api/waitlist') {
      if (request.method === 'GET') return new Response(null, { status: 204 })
      if (request.method === 'POST') return handleJoin(request, env)
      return json(405, { ok: false, error: 'method not allowed' })
    }
    return env.ASSETS.fetch(request)
  },
}
