// The live socket's send queue: a command issued while /ws is connecting or
// between reconnects (a session replacement closes every socket) is held and
// sent once the socket opens, instead of vanishing.

import { afterEach, beforeEach, expect, test } from 'bun:test'
import { api, openLiveSocket } from '../src/lib/api'

class FakeSocket {
  static OPEN = 1
  static CONNECTING = 0
  static instances: FakeSocket[] = []
  readyState = 0
  sent: string[] = []
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onmessage: ((ev: MessageEvent) => void) | null = null
  onerror: (() => void) | null = null
  constructor(public url: string) { FakeSocket.instances.push(this) }
  send(data: string) { this.sent.push(data) }
  open() { this.readyState = FakeSocket.OPEN; this.onopen?.() }
  drop() { this.readyState = 3; this.onclose?.() }
  close() { this.readyState = 3 }
}

const g = globalThis as unknown as { WebSocket: unknown; window: unknown; setTimeout: unknown; fetch: unknown }
const realWebSocket = g.WebSocket
const realWindow = g.window
const realSetTimeout = g.setTimeout
const realFetch = g.fetch

beforeEach(() => {
  FakeSocket.instances = []
  g.WebSocket = FakeSocket
  g.window = { location: { protocol: 'http:', host: 'localhost' } }
})
afterEach(() => {
  g.WebSocket = realWebSocket
  g.window = realWindow
  g.setTimeout = realSetTimeout
  g.fetch = realFetch
})

const handlers = () => ({ onOpen: () => {}, onMessage: () => {}, onClose: () => {} })
const types = (socket: FakeSocket) => socket.sent.map(s => (JSON.parse(s) as { type: string }).type)

test('a command sent before the socket opens is sent, in order, on open', () => {
  const live = openLiveSocket(handlers())
  const socket = FakeSocket.instances[0]
  live.send({ type: 'Play' })
  live.send({ type: 'Pause' })
  expect(socket.sent).toEqual([])
  socket.open()
  expect(types(socket)).toEqual(['Play', 'Pause'])
  live.send({ type: 'Step', dt: 0.001 })
  expect(socket.sent).toHaveLength(3)
  live.close()
})

test('a command sent between reconnects waits for the next socket', () => {
  // Run the 2 s reconnect timer immediately.
  g.setTimeout = (fn: () => void) => { fn(); return 0 }
  const live = openLiveSocket(handlers())
  FakeSocket.instances[0].open()
  FakeSocket.instances[0].drop()
  live.send({ type: 'Play' })
  const next = FakeSocket.instances[1]
  expect(next).toBeDefined()
  expect(FakeSocket.instances[0].sent).toEqual([])
  next.open()
  expect(types(next)).toEqual(['Play'])
  live.close()
})

test("a launch holds commands, drops the old session's socket and sends them on the new one", async () => {
  const live = openLiveSocket(handlers())
  const old = FakeSocket.instances[0]
  old.open()
  let answer: (r: Response) => void = () => {}
  g.fetch = () => new Promise<Response>(resolve => { answer = resolve })
  const launching = api.liveLaunch(new FormData())
  // The page still shows the old session as connected, and the user presses Play.
  live.send({ type: 'Play' })
  expect(old.sent).toEqual([])
  answer(new Response(JSON.stringify({ ok: true, board_name: 'b.kicad_pcb', replaced: true })))
  await launching
  const fresh = FakeSocket.instances[1]
  expect(fresh).toBeDefined()
  expect(old.readyState).toBe(3)
  expect(old.sent).toEqual([])
  fresh.open()
  expect(types(fresh)).toEqual(['Play'])
  live.close()
})

test('a refused launch leaves the old session standing and releases held commands to it', async () => {
  const live = openLiveSocket(handlers())
  const socket = FakeSocket.instances[0]
  socket.open()
  g.fetch = () => Promise.resolve(new Response(JSON.stringify({ ok: false, error: 'no' })))
  const launching = api.liveLaunch(new FormData())
  live.send({ type: 'Pause' })
  expect(socket.sent).toEqual([])
  await launching
  expect(FakeSocket.instances).toHaveLength(1)
  expect(types(socket)).toEqual(['Pause'])
  live.close()
})

test('close() discards anything still queued', () => {
  const live = openLiveSocket(handlers())
  live.send({ type: 'Play' })
  live.close()
  FakeSocket.instances[0].open()
  expect(FakeSocket.instances[0].sent).toEqual([])
})
