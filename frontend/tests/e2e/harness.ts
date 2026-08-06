const FIXTURE_URL = /\[visual-lint fixture server\]\s+(http:\/\/127\.0\.0\.1:\d+)/

/**
 * Wait until the fixture server announces the actual port selected by the OS.
 * Keeping the server on port 0 avoids collisions with local development and
 * parallel CI jobs; watching both stdout and process exit prevents a dead
 * child from consuming the whole job timeout.
 */
export function waitForFixtureBase(
  stdout: ReadableStream<Uint8Array>,
  exited: Promise<number>,
  timeoutMs: number,
  onOutput: (chunk: string) => void,
): Promise<string> {
  return new Promise((resolve, reject) => {
    let settled = false
    let transcript = ''
    const decoder = new TextDecoder()

    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      reject(new Error(`fixture server did not announce its URL within ${timeoutMs}ms`))
    }, timeoutMs)

    const succeed = (base: string) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(base)
    }

    const fail = (error: Error) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      reject(error)
    }

    void exited.then(code => {
      fail(new Error(`fixture server exited with code ${code} before announcing its URL`))
    })

    void (async () => {
      const reader = stdout.getReader()
      try {
        while (true) {
          const { done, value } = await reader.read()
          if (done) break
          const chunk = decoder.decode(value, { stream: true })
          onOutput(chunk)
          transcript += chunk
          const match = transcript.match(FIXTURE_URL)
          if (match?.[1]) succeed(match[1])
          if (transcript.length > 4_096) transcript = transcript.slice(-2_048)
        }
        const tail = decoder.decode()
        if (tail) {
          onOutput(tail)
          transcript += tail
          const match = transcript.match(FIXTURE_URL)
          if (match?.[1]) succeed(match[1])
        }
        fail(new Error('fixture server closed stdout before announcing its URL'))
      } catch (error) {
        fail(error instanceof Error ? error : new Error(String(error)))
      } finally {
        reader.releaseLock()
      }
    })()
  })
}

type KillableProcess = {
  exited: Promise<number>
  kill(signal?: number): void
}

/** Stop a child and wait for it, escalating only when graceful shutdown stalls. */
export async function terminateProcess(child: KillableProcess, timeoutMs = 5_000): Promise<void> {
  let exited = false
  const observedExit = child.exited.then(() => { exited = true })
  child.kill()
  let timer: ReturnType<typeof setTimeout> | undefined
  await Promise.race([
    observedExit,
    new Promise<void>(resolve => { timer = setTimeout(resolve, timeoutMs) }),
  ])
  if (timer) clearTimeout(timer)
  if (!exited) {
    child.kill(9)
    await child.exited
  }
}
