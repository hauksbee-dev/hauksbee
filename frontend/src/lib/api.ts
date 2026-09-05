// The shapes every call to the local engine repeats.

/** What to show a user for a thrown value of unknown type. */
export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/** Fetch aborts (a newer run superseded this one) are expected, not errors. */
export function isAbort(e: unknown): boolean {
  return e instanceof Error && e.name === 'AbortError'
}

/** GET a JSON endpoint, refusing anything but a 2xx. */
export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) throw new Error(`the server answered ${res.status} ${res.statusText}`)
  return await res.json() as T
}

/** POST a JSON body and read the JSON answer, whatever the status: these
 *  endpoints report their own refusals in the body. */
export async function postJson<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
  return await res.json() as T
}

/**
 * Read a response body ONCE as text, then parse it defensively.
 *
 * A stale build, a proxy, or a body-limit refusal answers in plaintext, and
 * `res.json()` on that throws a SyntaxError that hides the real message. The
 * status is only consulted when there is no parseable body, so an endpoint
 * that reports its refusal as JSON still reaches the caller intact.
 */
export async function readJson<T>(res: Response): Promise<T> {
  const text = await res.text()
  try {
    return JSON.parse(text) as T
  } catch {
    throw new Error(
      text.trim().slice(0, 400)
      || `${res.status} ${res.statusText}`
      || 'the server returned an empty or non-JSON response',
    )
  }
}
