#!/usr/bin/env bun
// Mock galvani-server: streams SimFrames at 30fps over WebSocket.
// Protocol mirrors crates/tarski-server/src/protocol.rs: serde tag="type".
//
// Usage: bun run mock-server.ts
//        (or: bun run mock-server from package.json)

import { serve } from 'bun'

const PORT = 3002

// ── Fake net names that would come from a real board extraction ──
const NET_NAMES = [
  'VCC', 'GND', '/MCLR', '/RB0', '/RB1', '/RB2', '/RB3', '/RB4', '/RB5',
  '/RC0', '/RC1', '/RC2', 'CLK', 'DATA', 'SPI_CS', 'SPI_MOSI', 'SPI_MISO',
  '/ICSP_CLK', '/ICSP_DAT', 'VREF', 'AGND', 'DVDD', 'AVDD',
]

const COMP_REFS = ['U1', 'U2', 'C1', 'C2', 'C3', 'R1', 'R2', 'R3', 'R4', 'LED1', 'J1', 'J2']

// ── State ──
let time_ms = 0
let timestep = 0
let running = false
let speed = 1.0
let phase = 0

// ── Client connections ──
const clients = new Set<import('bun').ServerWebSocket<unknown>>()

// ── Board info (sent once on connect) ──
function boardInfo() {
  return JSON.stringify({
    type: 'BoardInfo',
    num_nets: NET_NAMES.length,
    num_components: COMP_REFS.length,
    board_file: '/boards/pic_programmer.kicad_pcb',
  })
}

// ── Sim frame generator ──
function makeFrame() {
  // Generate interesting voltage patterns
  const net_voltages: Record<string, number> = {}
  NET_NAMES.forEach((name, i) => {
    const base = name === 'VCC' ? 5.0
      : name === 'GND' ? 0.0
      : name.includes('CLK') ? Math.sin(phase * 3 + i) > 0 ? 3.3 : 0
      : name.includes('DATA') ? Math.sin(phase * 7 + i) > 0 ? 3.3 : 0
      : name.startsWith('/R') ? (Math.sin(phase + i * 0.7) * 0.5 + 1.5)
      : (Math.sin(phase * 0.5 + i) * 0.8 + 1.6)
    net_voltages[name] = parseFloat(base.toFixed(4))
  })

  // Signal particles — positions along nets (t ∈ [0, 1])
  const signal_particles: Record<string, number[]> = {}
  if (running) {
    const flowNets = ['CLK', 'DATA', '/ICSP_CLK']
    flowNets.forEach((n, ni) => {
      const count = 3
      const ts: number[] = []
      for (let i = 0; i < count; i++) {
        ts.push(((phase * 0.4 + i / count + ni * 0.1) % 1 + 1) % 1)
      }
      signal_particles[n] = ts
    })
  }

  // Component states
  const component_states: Record<string, object> = {}
  COMP_REFS.forEach(ref => {
    if (ref.startsWith('U')) {
      component_states[ref] = { kind: 'Generic', label: running ? 'active' : 'idle' }
    } else if (ref === 'LED1') {
      component_states[ref] = {
        kind: 'Generic',
        label: net_voltages['VCC']! > 4.5 ? 'ON' : 'OFF',
      }
    }
  })

  return JSON.stringify({
    type: 'SimFrame',
    time_ms: parseFloat(time_ms.toFixed(3)),
    timestep,
    running,
    speed,
    net_voltages,
    component_states,
    signal_particles,
  })
}

// ── Handle client messages ──
function handleMessage(ws: import('bun').ServerWebSocket<unknown>, raw: string) {
  let msg: { type: string; [k: string]: unknown }
  try { msg = JSON.parse(raw) as { type: string; [k: string]: unknown } }
  catch { return }

  switch (msg.type) {
    case 'Play':    running = true; console.log('[mock] play'); break
    case 'Pause':   running = false; console.log('[mock] pause'); break
    case 'Step':    if (!running) { time_ms += 1 / 30; timestep++ }; break
    case 'SetSpeed': speed = (msg.speed as number) ?? 1; break
    case 'GetBoardInfo': ws.send(boardInfo()); break
    case 'AddProbe': console.log(`[mock] probe: ${msg.net_name}`); break
    case 'RemoveProbe': console.log(`[mock] remove probe: ${msg.net_name}`); break
  }
}

// ── 30fps broadcast loop ──
const FRAME_MS = 1000 / 30
setInterval(() => {
  if (clients.size === 0) return

  if (running) {
    time_ms += (FRAME_MS / 1000) * speed
    timestep++
    phase += 0.06 * speed
  }

  const frame = makeFrame()
  for (const ws of clients) {
    try { ws.send(frame) } catch { clients.delete(ws) }
  }
}, FRAME_MS)

// ── Bun server ──
serve({
  port: PORT,
  websocket: {
    open(ws) {
      clients.add(ws)
      console.log(`[mock] client connected (total: ${clients.size})`)
      ws.send(boardInfo())
    },
    close(ws) {
      clients.delete(ws)
      console.log(`[mock] client disconnected (total: ${clients.size})`)
    },
    message(ws, data) {
      handleMessage(ws, typeof data === 'string' ? data : data.toString())
    },
  },
  fetch(req, server) {
    const url = new URL(req.url)
    if (url.pathname === '/ws' || url.pathname === '/') {
      if (server.upgrade(req)) return undefined
    }
    return new Response('galvani mock-server', { status: 200 })
  },
})

console.log(`[galvani mock-server] listening on ws://localhost:${PORT}/ws`)
console.log('[mock] send { "type": "Play" } to start the sim')
