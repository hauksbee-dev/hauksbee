// Starting a real `hauksbee serve` for a browser flow: a port nobody else
// holds, and a readiness wait that fails fast when the binary dies instead of
// burning the job timeout. Shared, so the on-demand live flows cannot drift
// into different definitions of "the server is up".

/** Reserve, then release, a port the OS picked: parallel flows and local
 *  development never collide on a hard-coded number. */
export async function freePort(): Promise<number> {
  const holder = Bun.serve({ port: 0, fetch: () => new Response('reserved') })
  const port = holder.port
  await holder.stop(true)
  if (port === undefined) throw new Error('Bun did not assign a local test port')
  return port
}

/** Wait until the server answers /api/startup, or until it exits or the
 *  deadline passes — whichever comes first. */
export async function waitForStartup(base: string, child: Bun.Subprocess, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const exited = await Promise.race([
      child.exited.then(code => ({ exited: true as const, code })),
      Bun.sleep(150).then(() => ({ exited: false as const, code: 0 })),
    ])
    if (exited.exited) throw new Error(`hauksbee serve exited early with code ${exited.code}`)
    try {
      const response = await fetch(`${base}/api/startup`)
      if (response.ok) return
    } catch { /* server is still starting */ }
  }
  throw new Error(`hauksbee serve did not become ready within ${timeoutMs} ms`)
}
