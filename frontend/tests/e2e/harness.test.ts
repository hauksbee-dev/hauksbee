import { describe, expect, test } from 'bun:test'
import { terminateProcess, waitForFixtureBase } from './harness'

const encoder = new TextEncoder()

function output(...chunks: string[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk))
      controller.close()
    },
  })
}

function pendingExit(): Promise<number> {
  return new Promise(() => undefined)
}

async function rejectionMessage(operation: Promise<unknown>): Promise<string> {
  try {
    await operation
  } catch (error) {
    return error instanceof Error ? error.message : String(error)
  }
  throw new Error('expected operation to reject')
}

describe('end-to-end fixture readiness', () => {
  test('discovers the operating-system-selected port from split output', async () => {
    const seen: string[] = []
    const base = await waitForFixtureBase(
      output('[visual-lint fixture ', 'server] http://127.0.0.1:43127\n'),
      pendingExit(),
      1_000,
      chunk => seen.push(chunk),
    )

    expect(base).toBe('http://127.0.0.1:43127')
    expect(seen.join('')).toContain('fixture server')
  })

  test('fails promptly when the fixture process exits before readiness', async () => {
    const stillOpen = new ReadableStream<Uint8Array>()
    const message = await rejectionMessage(
      waitForFixtureBase(stillOpen, Promise.resolve(23), 1_000, () => undefined),
    )
    expect(message).toContain('exited with code 23 before announcing its URL')
  })

  test('times out instead of letting a silent fixture hang CI', async () => {
    const stillOpen = new ReadableStream<Uint8Array>()
    const message = await rejectionMessage(
      waitForFixtureBase(stillOpen, pendingExit(), 20, () => undefined),
    )
    expect(message).toContain('did not announce its URL within 20ms')
  })
})

describe('end-to-end child cleanup', () => {
  test('waits for graceful process exit before returning', async () => {
    let finish!: (code: number) => void
    const exited = new Promise<number>(resolve => { finish = resolve })
    let killed = false

    await terminateProcess({
      exited,
      kill() {
        killed = true
        queueMicrotask(() => finish(0))
      },
    }, 100)

    expect(killed).toBe(true)
  })

  test('force-kills a child that ignores graceful shutdown', async () => {
    let finish!: (code: number) => void
    const exited = new Promise<number>(resolve => { finish = resolve })
    const signals: Array<number | undefined> = []

    await terminateProcess({
      exited,
      kill(signal) {
        signals.push(signal)
        if (signal === 9) finish(137)
      },
    }, 5)

    expect(signals).toEqual([undefined, 9])
  })
})
