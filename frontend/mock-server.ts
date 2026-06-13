#!/usr/bin/env bun
// Mock hauksbee-server: streams SimFrames at 30fps over WebSocket.
// Protocol exactly mirrors crates/hauksbee-server/src/protocol.rs (serde tag="type").
//
// Usage: bun run mock-server.ts

import { serve } from 'bun'

const PORT = Number(process.env.MOCK_PORT ?? 3002)

const DEFAULT_CONTROLS = {
  temperature_c: 27.0,
  parasitics: false,
  junction_caps: true,
  tolerances: false,
  integration: 'trap',
  fixed_dt: 0.0,
  granularity: 1.0,
}

// ── Fault demo mode ──
// Set MOCK_FAULTS=1 to stream a scripted fault sequence:
//   t=0..2s  → stress climbing on R1
//   t>2s     → FaultInfo destroyed on R1
const FAULT_MODE = process.env.MOCK_FAULTS === '1'

// ── State ──
let sim_time = 0
let running = false
let speed = 1.0
let phase = 0
let controls = { ...DEFAULT_CONTROLS }
let adc_volts = 2.5

// ── Client connections ──
const clients = new Set<import('bun').ServerWebSocket<unknown>>()

// ── Board info ──
function boardInfo() {
  // Fault mode: expose R1 component so fault highlight is visible
  if (FAULT_MODE) {
    return JSON.stringify({
      type: 'BoardInfo',
      name: process.env.MOCK_BOARD ?? 'pic_programmer',
      board_url: `/boards/${process.env.MOCK_BOARD ?? 'pic_programmer'}.kicad_pcb`,
      num_components: 5,
      num_nets: 4,
      nets: ['D13_LED', 'A0', 'VCC', 'GND'],
      component_kinds: { U1: 'mcu', R1: 'resistor', C1: 'capacitor' },
      mcus: [],
    })
  }
  return JSON.stringify({
    type: 'BoardInfo',
    name: process.env.MOCK_BOARD ?? 'demo',
    board_url: `/boards/${process.env.MOCK_BOARD ?? 'demo'}.kicad_pcb`,
    num_components: 1,
    num_nets: 2,
    nets: ['D13_LED', 'A0'],
    component_kinds: { U1: 'mcu' },
    mcus: [['U1', 'simavr:atmega328p']],
  })
}

function statusMsg() {
  return JSON.stringify({
    type: 'Status',
    running,
    sim_time,
    options: controls,
  })
}

// ── Sim frame generator ──
function makeFaults(): { component: string; fault_kind: string; value: number; limit: number; t: number }[] {
  if (!FAULT_MODE) return []
  // Ramp: stress increases from t=0.5s; fault triggers at t=2s
  if (sim_time < 0.5) return []
  if (sim_time < 2.5) {
    // Pre-fault: only stress state (no fault entry yet — shown via component heat in 2D)
    return []
  }
  // Post 2.5s: R1 destroyed
  return [{
    component: 'R1',
    fault_kind: 'overpower',
    value: parseFloat((4.8 + Math.random() * 0.4).toFixed(3)),
    limit: 0.25,
    t: parseFloat(sim_time.toFixed(4)),
  }]
}

function makeFrame() {
  const led_high = phase % (Math.PI * 2) < Math.PI  // ~5Hz blink
  const net_voltages: Record<string, number> = {
    D13_LED: led_high ? 5.0 : 0.0,
    A0: adc_volts,
  }

  const component_states: Record<string, Record<string, number>> = {
    U1: { running: 1.0 },
  }

  // Fault mode: show R1 heating up before the fault fires
  if (FAULT_MODE && sim_time > 0.5) {
    const stress = Math.min(1, (sim_time - 0.5) / 2.0)
    component_states['R1'] = { dissipation_mw: stress * 600 }
  }

  const faults = makeFaults()

  return JSON.stringify({
    type: 'SimFrame',
    t: parseFloat(sim_time.toFixed(6)),
    realtime_factor: 1.0,
    net_voltages,
    component_states,
    uart: {},
    net_currents: {},
    ...(faults.length > 0 ? { faults } : {}),
  })
}

// ── Handle client messages ──
function handleMessage(ws: import('bun').ServerWebSocket<unknown>, raw: string) {
  let msg: { type: string; [k: string]: unknown }
  try { msg = JSON.parse(raw) as { type: string; [k: string]: unknown } }
  catch { return }

  switch (msg.type) {
    case 'Play':
      running = true
      console.log('[mock] play')
      break
    case 'Pause':
      running = false
      console.log('[mock] pause')
      break
    case 'Step': {
      const dt = (msg.dt as number) || (1 / 30)
      sim_time += dt
      phase += dt * 2 * Math.PI * 5  // 5Hz
      ws.send(makeFrame())
      break
    }
    case 'Reset':
      running = false
      sim_time = 0
      phase = 0
      console.log('[mock] reset')
      break
    case 'SetSpeed':
      speed = (msg.factor as number) ?? 1
      console.log(`[mock] speed: ${speed}x`)
      break
    case 'SetControls':
      controls = {
        temperature_c: (msg.temperature_c as number) ?? controls.temperature_c,
        parasitics: (msg.parasitics as boolean) ?? controls.parasitics,
        junction_caps: (msg.junction_caps as boolean) ?? controls.junction_caps,
        tolerances: (msg.tolerances as boolean) ?? controls.tolerances,
        integration: (msg.integration as string) ?? controls.integration,
        fixed_dt: (msg.fixed_dt as number) ?? controls.fixed_dt,
        granularity: (msg.granularity as number) ?? controls.granularity,
      }
      break
    case 'Serial': {
      const mcu = (msg.mcu as string) ?? 'U1'
      const data = (msg.data as number[]) ?? []
      const text = new TextDecoder().decode(new Uint8Array(data))
      console.log(`[mock] serial <- ${JSON.stringify(text)}`)
      // Demo responses
      const responses: Record<string, string> = {
        'i\n': 'hauksbee-demo v1\r\n',
        'v\n': `${Math.round(adc_volts * 1000)} mV\r\n`,
      }
      const resp = responses[text]
      if (resp) {
        const respBytes = Array.from(new TextEncoder().encode(resp))
        setTimeout(() => {
          ws.send(JSON.stringify({
            type: 'SimFrame',
            t: sim_time,
            realtime_factor: 1.0,
            net_voltages: { D13_LED: 0, A0: adc_volts },
            component_states: { U1: { running: 1.0 } },
            uart: { [mcu]: respBytes },
            net_currents: {},
          }))
        }, 20)
      }
      break
    }
    case 'SetInput':
      if (msg.source === 'A0') {
        adc_volts = (msg.value as number) ?? 2.5
        console.log(`[mock] SetInput A0=${adc_volts}V`)
      }
      break
    case 'AddProbe':
      console.log(`[mock] AddProbe: ${msg.net}`)
      break
    case 'RemoveProbe':
      console.log(`[mock] RemoveProbe: ${msg.net}`)
      break
  }

  // Always broadcast status after a command
  for (const c of clients) {
    try { c.send(statusMsg()) } catch { clients.delete(c) }
  }
}

// ── 30fps broadcast loop ──
const FRAME_MS = 1000 / 30
const BLINK_HZ = 5
setInterval(() => {
  if (clients.size === 0) return

  if (running) {
    const dt = (FRAME_MS / 1000) * speed
    sim_time += dt
    phase += dt * 2 * Math.PI * BLINK_HZ
  }

  const frame = makeFrame()
  const status = statusMsg()
  for (const ws of clients) {
    try {
      ws.send(frame)
      ws.send(status)
    } catch { clients.delete(ws) }
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
      ws.send(statusMsg())
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
    return new Response('hauksbee mock-server', { status: 200 })
  },
})

console.log(`[hauksbee mock-server] listening on ws://localhost:${PORT}/ws`)
console.log('[mock] send { "type": "Play" } to start the sim')
