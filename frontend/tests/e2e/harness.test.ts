import { afterEach, describe, expect, mock, spyOn, test } from 'bun:test'
import { terminateProcess, waitForFixtureBase, withFixture } from './harness'

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

describe('shared fixture ownership', () => {
  afterEach(() => mock.restore())

  test('external base does not spawn a fixture', async () => {
    const spawn = spyOn(Bun, 'spawn')
    spyOn(globalThis, 'fetch').mockResolvedValue(new Response('ready'))
    const reached: string[] = []
    await withFixture(async (base) => {
      reached.push(base)
    }, 'http://127.0.0.1:1234')
    expect(reached).toEqual(['http://127.0.0.1:1234'])
    expect(spawn).not.toHaveBeenCalled()
  })

  test('external readiness timeout never enters the browser flow', async () => {
    spyOn(globalThis, 'fetch').mockResolvedValue(new Response('not ready', { status: 503 }))
    spyOn(Bun, 'sleep').mockResolvedValue(undefined)
    const run = mock(async () => undefined)
    expect(await rejectionMessage(withFixture(run, 'http://127.0.0.1:1234', 0, 1))).toContain(
      'nothing answering',
    )
    expect(run).not.toHaveBeenCalled()
  })

  test('announced port, explicit overrides and output survive browser launch failure', async () => {
    const write = spyOn(process.stdout, 'write').mockReturnValue(true)
    const spawn = spyOn(Bun, 'spawn')
    spyOn(globalThis, 'fetch').mockResolvedValue(new Response('ready'))
    for (const port of [0, 4321]) {
      let finish!: (code: number) => void
      const child = {
        stdout: output('[visual-lint fixture server] http://127.0.0.1:43127\n'),
        exited: new Promise<number>((resolve) => {
          finish = resolve
        }),
        kill: mock(() => finish(0)),
      }
      spawn.mockReturnValue(child as never)
      const reason = await rejectionMessage(
        withFixture(
          async (base) => {
            expect(base).toBe('http://127.0.0.1:43127')
            throw new Error('browser launch failed')
          },
          null,
          port,
        ),
      )
      expect(reason).toContain('browser launch failed')
      expect(spawn.mock.calls.at(-1)?.[0]).toContain(String(port))
      expect(spawn.mock.calls.at(-1)?.[1]).toEqual({ stdout: 'pipe', stderr: 'inherit' })
      expect(write).toHaveBeenCalledWith(expect.stringContaining('43127'))
      expect(child.kill).toHaveBeenCalledTimes(1)
    }
  })

  test('early fixture exit is propagated and reaped', async () => {
    const child = { stdout: output('startup failed\n'), exited: Promise.resolve(23), kill: mock() }
    spyOn(Bun, 'spawn').mockReturnValue(child as never)
    const run = mock(async () => undefined)
    expect(await rejectionMessage(withFixture(run, null, 0))).toMatch(/exited|closed stdout/)
    expect(run).not.toHaveBeenCalled()
    expect(child.kill).toHaveBeenCalledTimes(1)
  })
})
